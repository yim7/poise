use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use poise_application::{
    RecoveryAnomalyObserver, TrackInstrument, TrackMutationError, TrackRecoveryIssue,
};
use poise_core::track::TrackId;
use poise_engine::manager::ExchangeSyncMode;
use poise_engine::observation::CompleteOpenOrderSnapshot;
use poise_engine::ports::ExecutionPort;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, sleep};

use crate::order_outcome::reconcile_execution;
use crate::server_context::ReconcileState;

use super::{
    ReconcileExecution, ReconcileRequest, ReconcileStateAccess, ServerRuntime,
    diagnostics::{describe_open_orders, describe_runtime_bindings},
    exchange_state, preserve_track_mutation_error,
};

const ORDER_SET_RECOVERY_RESET_RETRY_ATTEMPTS: usize = 5;
#[cfg(test)]
const ORDER_SET_RECOVERY_RESET_RETRY_DELAY: Duration = Duration::from_millis(1);
#[cfg(not(test))]
const ORDER_SET_RECOVERY_RESET_RETRY_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
struct RecoveryTrackedTrack {
    instrument: poise_core::track::Instrument,
    next_retry_at: Instant,
}

#[derive(Default)]
pub(crate) struct RecoveryDirtyState {
    workset: Mutex<RecoveryWorkset>,
    notify: Notify,
}

impl RecoveryDirtyState {
    pub(crate) fn mark_recovery_anomaly(
        &self,
        track_id: &poise_core::track::TrackId,
        active: bool,
    ) {
        self.workset
            .lock()
            .unwrap()
            .anomaly_updates
            .insert(track_id.as_str().to_string(), active);
        self.notify.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn mark_reseed_required(&self) {
        self.workset.lock().unwrap().reseed_required = true;
        self.notify.notify_one();
    }

    fn take(&self) -> RecoveryWorkset {
        std::mem::take(&mut *self.workset.lock().unwrap())
    }

    async fn wait(&self) {
        self.notify.notified().await;
    }
}

pub(crate) struct RecoveryAnomalyDirtyObserver {
    dirty_state: Arc<RecoveryDirtyState>,
}

impl RecoveryAnomalyDirtyObserver {
    pub(crate) fn new(dirty_state: Arc<RecoveryDirtyState>) -> Self {
        Self { dirty_state }
    }
}

impl RecoveryAnomalyObserver for RecoveryAnomalyDirtyObserver {
    fn observe_recovery_anomaly_change(&self, track_id: &poise_core::track::TrackId, active: bool) {
        self.dirty_state.mark_recovery_anomaly(track_id, active);
    }
}

pub(super) fn spawn_recovery_task(
    runtime: &ServerRuntime,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.clone();
    let execution = Arc::clone(&runtime.execution);
    let retry_interval = runtime.recovery_retry_interval;
    let audit_interval = runtime.audit_interval;

    tokio::spawn(async move {
        let instruments = state
            .reconcile
            .observation_service
            .track_instruments()
            .await;
        let mut tracked =
            seed_recovery_tracking(&state.reconcile, &instruments, retry_interval).await;
        let mut next_audit_at = instruments
            .iter()
            .map(|track| (track.id.clone(), Instant::now() + audit_interval))
            .collect::<HashMap<_, _>>();
        let mut pending_workset = RecoveryWorkset::default();
        let mut ticker = tokio::time::interval(Duration::from_millis(50));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            pending_workset.merge(state.reconcile.recovery_dirty_state.take());

            if !pending_workset.is_empty() {
                apply_recovery_workset(
                    &state.reconcile,
                    &instruments,
                    &mut tracked,
                    &mut pending_workset,
                    retry_interval,
                )
                .await;
                continue;
            }

            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let due_anomaly_tracks: Vec<(String, poise_core::track::Instrument)> = tracked
                        .iter()
                        .filter(|(_, tracked_track)| tracked_track.next_retry_at <= now)
                        .map(|(track_id, tracked_track)| (track_id.clone(), tracked_track.instrument.clone()))
                        .collect();
                    let due_audit_tracks: Vec<(String, poise_core::track::Instrument)> = instruments
                        .iter()
                        .filter(|track| {
                            next_audit_at
                                .get(&track.id)
                                .is_some_and(|next_audit| *next_audit <= now)
                        })
                        .map(|track| (track.id.clone(), track.instrument.clone()))
                        .collect();

                    let mut due_tracks = due_audit_tracks.into_iter().collect::<HashMap<_, _>>();
                    for (track_id, instrument) in due_anomaly_tracks {
                        due_tracks.insert(track_id, instrument);
                    }

                    for (track_id, instrument) in due_tracks {
                        if let Some(tracked_track) = tracked.get_mut(&track_id) {
                            tracked_track.next_retry_at = Instant::now() + retry_interval;
                        }
                        next_audit_at.insert(track_id.clone(), Instant::now() + audit_interval);
                        if let Err(error) = sync_exchange_state_from_exchange(
                            &state.reconcile,
                            execution.as_ref(),
                            &track_id,
                            &instrument,
                            ExchangeSyncMode::RecoverAndReconcile,
                        )
                        .await {
                            tracing::warn!(
                                "failed to auto-resync recovery anomaly for {}: {}",
                                instrument.symbol,
                                error.message()
                            );
                        }
                    }
                }
                _ = state.reconcile.recovery_dirty_state.wait() => {
                    pending_workset.merge(state.reconcile.recovery_dirty_state.take());
                }
            }
        }
    })
}

pub(super) async fn enqueue_reconcile_request(
    state: &impl ReconcileStateAccess,
    execution: &dyn ExecutionPort,
    request: ReconcileRequest,
    instrument: &poise_core::track::Instrument,
) -> std::result::Result<ReconcileExecution, TrackMutationError> {
    let reconcile_execution = reconcile_execution(&request.track_id, vec![request.reason]);
    let state = state.reconcile_state_view();
    sync_exchange_state_from_exchange(
        &state,
        execution,
        &request.track_id,
        instrument,
        ExchangeSyncMode::RecoverAndReconcile,
    )
    .await?;
    Ok(reconcile_execution)
}

pub(super) async fn sync_exchange_state_from_exchange(
    state: &impl ReconcileStateAccess,
    execution: &dyn ExecutionPort,
    track_id: &str,
    instrument: &poise_core::track::Instrument,
    mode: ExchangeSyncMode,
) -> std::result::Result<(), TrackMutationError> {
    let state = state.reconcile_state_view();
    let _reconcile_guard = state.reconcile_guards.lock(track_id).await;
    let sync_token = state.exchange_freshness.prepare_sync(track_id).await;
    let recovery_view = state
        .runtime_lifecycle_service
        .load_track_recovery_summary(&TrackId::new(track_id))
        .await
        .map_err(TrackMutationError::Persistence)?;
    let mut position = execution
        .get_position(instrument)
        .await
        .map_err(TrackMutationError::Persistence)?;
    let mut open_orders = execution
        .get_open_orders(instrument)
        .await
        .map_err(TrackMutationError::Persistence)?;

    if recovery_view
        .as_ref()
        .and_then(|view| view.issue)
        .is_some_and(order_set_reset_required_for_recovery_issue)
    {
        if !open_orders.is_empty() {
            let runtime = state
                .observation_service
                .track_runtime_view(track_id)
                .await
                .ok()
                .flatten();
            tracing::warn!(
                track_id,
                symbol = %instrument.symbol,
                recovery_issue = ?recovery_view.as_ref().and_then(|view| view.issue),
                exchange_open_orders = ?describe_open_orders(&open_orders),
                local_bindings = ?describe_runtime_bindings(runtime.as_ref()),
                "order-set recovery anomaly encountered; resetting instrument open orders"
            );
            reset_open_orders_for_order_set_recovery_anomaly(execution, instrument).await?;
            position = execution
                .get_position(instrument)
                .await
                .map_err(TrackMutationError::Persistence)?;
            open_orders = execution
                .get_open_orders(instrument)
                .await
                .map_err(TrackMutationError::Persistence)?;
        }
    }

    let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(
        open_orders
            .orders()
            .iter()
            .map(exchange_state::order_observation)
            .collect(),
    );

    if matches!(mode, ExchangeSyncMode::RecoverAndReconcile) {
        let _ = state
            .observation_service
            .sync_exchange_state(
                track_id,
                exchange_state::position_observation(&position),
                open_orders,
            )
            .await
            .map_err(preserve_track_mutation_error)?;
    } else {
        let _ = state
            .observation_service
            .sync_exchange_state_without_reconcile(
                track_id,
                exchange_state::position_observation(&position),
                open_orders,
            )
            .await
            .map_err(preserve_track_mutation_error)?;
    }
    state.exchange_freshness.clear_if_current(sync_token).await;
    Ok(())
}

fn order_set_reset_required_for_recovery_issue(issue: TrackRecoveryIssue) -> bool {
    matches!(
        issue,
        TrackRecoveryIssue::UnknownLiveOrder
            | TrackRecoveryIssue::DuplicateLiveOrders
            | TrackRecoveryIssue::AmbiguousLiveOrder
    )
}

async fn reset_open_orders_for_order_set_recovery_anomaly(
    execution: &dyn ExecutionPort,
    instrument: &poise_core::track::Instrument,
) -> std::result::Result<(), TrackMutationError> {
    execution
        .cancel_all(instrument)
        .await
        .with_context(|| {
            format!(
                "failed to cancel all live orders for order-set recovery anomaly on {}",
                instrument.symbol
            )
        })
        .map_err(TrackMutationError::Persistence)?;

    for attempt in 0..ORDER_SET_RECOVERY_RESET_RETRY_ATTEMPTS {
        let remaining = execution
            .get_open_orders(instrument)
            .await
            .map_err(TrackMutationError::Persistence)?;
        if remaining.is_empty() {
            return Ok(());
        }
        if attempt + 1 == ORDER_SET_RECOVERY_RESET_RETRY_ATTEMPTS {
            return Err(TrackMutationError::Persistence(anyhow!(
                "order-set recovery reset left open orders on {} after cancel_all",
                instrument.symbol
            )));
        }
        sleep(ORDER_SET_RECOVERY_RESET_RETRY_DELAY).await;
    }

    Err(TrackMutationError::Persistence(anyhow!(
        "order-set recovery reset exhausted retries for {}",
        instrument.symbol
    )))
}

fn update_recovery_tracking(
    tracked: &mut HashMap<String, RecoveryTrackedTrack>,
    instruments: &[TrackInstrument],
    track_id: &str,
    recovery_anomaly_active: bool,
    retry_interval: Duration,
) {
    if !recovery_anomaly_active {
        tracked.remove(track_id);
        return;
    }

    let Some(instrument) = instruments
        .iter()
        .find(|track| track.id == track_id)
        .map(|track| track.instrument.clone())
    else {
        return;
    };

    tracked
        .entry(track_id.to_string())
        .or_insert_with(|| RecoveryTrackedTrack {
            instrument,
            next_retry_at: Instant::now() + retry_interval,
        });
}

async fn seed_recovery_tracking(
    state: &ReconcileState,
    instruments: &[TrackInstrument],
    retry_interval: Duration,
) -> HashMap<String, RecoveryTrackedTrack> {
    let mut tracked = HashMap::new();
    for track in instruments {
        let Ok(Some(recovery_view)) = state
            .runtime_lifecycle_service
            .load_track_recovery_summary(&TrackId::new(&track.id))
            .await
        else {
            continue;
        };
        update_recovery_tracking(
            &mut tracked,
            instruments,
            &track.id,
            recovery_view.issue.is_some(),
            retry_interval,
        );
    }
    tracked
}

async fn apply_recovery_workset(
    state: &ReconcileState,
    instruments: &[TrackInstrument],
    tracked: &mut HashMap<String, RecoveryTrackedTrack>,
    workset: &mut RecoveryWorkset,
    retry_interval: Duration,
) {
    let workset = std::mem::take(workset);

    if workset.reseed_required {
        *tracked = seed_recovery_tracking(state, instruments, retry_interval).await;
        return;
    }

    for (track_id, recovery_anomaly_active) in &workset.anomaly_updates {
        update_recovery_tracking(
            tracked,
            instruments,
            track_id.as_str(),
            *recovery_anomaly_active,
            retry_interval,
        );
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RecoveryWorkset {
    anomaly_updates: HashMap<String, bool>,
    reseed_required: bool,
}

impl RecoveryWorkset {
    fn is_empty(&self) -> bool {
        self.anomaly_updates.is_empty() && !self.reseed_required
    }

    fn merge(&mut self, other: RecoveryWorkset) {
        self.anomaly_updates.extend(other.anomaly_updates);
        self.reseed_required |= other.reseed_required;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Result, anyhow};
    use poise_application::{TrackEffectJournal, TrackMutationStore, TrackQueryStore};
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::{BindingStatus, RecoveryAnomaly};
    use poise_engine::manager::ExchangeSyncMode;
    use poise_engine::ports::{
        ExchangeOpenOrderSnapshot, ExchangeOrder, ExecutionPort, OrderReceipt, OrderRequest,
        OrderStatus, Position,
    };
    use poise_storage::sqlite::SqliteStorage;

    use super::*;
    use crate::test_support::{
        build_runtime_and_effect_worker_test_contexts, build_test_application_services,
        test_manager, unavailable_account_monitor,
    };

    #[test]
    fn recovery_workset_coalesces_track_marks() {
        let mut workset = RecoveryWorkset::default();
        workset.anomaly_updates.insert("BTCUSDT".to_string(), true);

        workset.merge(RecoveryWorkset {
            anomaly_updates: HashMap::from([
                ("BTCUSDT".to_string(), false),
                ("ETHUSDT".to_string(), true),
            ]),
            reseed_required: false,
        });

        assert_eq!(
            workset,
            RecoveryWorkset {
                anomaly_updates: HashMap::from([
                    ("BTCUSDT".to_string(), false),
                    ("ETHUSDT".to_string(), true),
                ]),
                reseed_required: false,
            }
        );
    }

    #[test]
    fn recovery_workset_coalesces_reseed_requests() {
        let mut workset = RecoveryWorkset {
            reseed_required: true,
            ..Default::default()
        };
        workset.merge(RecoveryWorkset {
            anomaly_updates: HashMap::from([("SOLUSDT".to_string(), true)]),
            reseed_required: true,
        });

        assert_eq!(
            workset,
            RecoveryWorkset {
                anomaly_updates: HashMap::from([("SOLUSDT".to_string(), true)]),
                reseed_required: true,
            }
        );
    }

    #[test]
    fn recovery_dirty_state_marks_reseed_requests() {
        let dirty_state = RecoveryDirtyState::default();

        dirty_state.mark_reseed_required();

        assert_eq!(
            dirty_state.take(),
            RecoveryWorkset {
                anomaly_updates: HashMap::new(),
                reseed_required: true,
            }
        );
    }

    #[tokio::test]
    async fn sync_exchange_state_resets_order_set_for_unknown_live_order() {
        assert_order_set_reset_for_recovery_anomaly(
            RecoveryAnomaly::UnknownLiveOrder,
            |tracked_request| {
                vec![
                    ExchangeOrder {
                        instrument: tracked_request.instrument.clone(),
                        order_id: "tracked-order".to_string(),
                        client_order_id: tracked_request.client_order_id.clone(),
                        side: tracked_request.side,
                        price: tracked_request.price,
                        qty: tracked_request.quantity,
                        filled_qty: 0.0,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                    ExchangeOrder {
                        instrument: tracked_request.instrument.clone(),
                        order_id: "unknown-order".to_string(),
                        client_order_id: "unknown-client".to_string(),
                        side: poise_core::types::Side::Sell,
                        price: tracked_request.price - 1.0,
                        qty: tracked_request.quantity,
                        filled_qty: 0.0,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                ]
            },
        )
        .await;
    }

    #[tokio::test]
    async fn sync_exchange_state_resets_order_set_for_duplicate_live_orders() {
        assert_order_set_reset_for_recovery_anomaly(
            RecoveryAnomaly::DuplicateLiveOrders,
            |tracked_request| {
                vec![
                    ExchangeOrder {
                        instrument: tracked_request.instrument.clone(),
                        order_id: "tracked-order-a".to_string(),
                        client_order_id: tracked_request.client_order_id.clone(),
                        side: tracked_request.side,
                        price: tracked_request.price,
                        qty: tracked_request.quantity,
                        filled_qty: 0.0,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                    ExchangeOrder {
                        instrument: tracked_request.instrument.clone(),
                        order_id: "tracked-order-b".to_string(),
                        client_order_id: tracked_request.client_order_id.clone(),
                        side: tracked_request.side,
                        price: tracked_request.price,
                        qty: tracked_request.quantity,
                        filled_qty: 0.0,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                ]
            },
        )
        .await;
    }

    #[tokio::test]
    async fn sync_exchange_state_resets_order_set_for_ambiguous_live_order() {
        assert_order_set_reset_for_recovery_anomaly(
            RecoveryAnomaly::AmbiguousLiveOrder,
            |tracked_request| {
                vec![
                    ExchangeOrder {
                        instrument: tracked_request.instrument.clone(),
                        order_id: "ambiguous-order".to_string(),
                        client_order_id: "ambiguous-client".to_string(),
                        side: tracked_request.side,
                        price: tracked_request.price,
                        qty: tracked_request.quantity,
                        filled_qty: 0.0,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                    ExchangeOrder {
                        instrument: tracked_request.instrument.clone(),
                        order_id: "tracked-order".to_string(),
                        client_order_id: tracked_request.client_order_id.clone(),
                        side: tracked_request.side,
                        price: tracked_request.price,
                        qty: tracked_request.quantity,
                        filled_qty: 0.0,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                ]
            },
        )
        .await;
    }

    async fn assert_order_set_reset_for_recovery_anomaly<F>(
        anomaly: RecoveryAnomaly,
        build_open_orders: F,
    ) where
        F: FnOnce(&OrderRequest) -> Vec<ExchangeOrder>,
    {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager("btc-core"),
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications);
        let (runtime_context, _effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectJournal>,
                account_monitor,
            );

        let transition = runtime_context
            .observe_market("btc-core", 95.0)
            .await
            .unwrap();
        let tracked_request = transition
            .effects
            .iter()
            .find_map(|effect| match effect {
                TrackEffect::SubmitOrder { request, .. } => Some(request.clone()),
                _ => None,
            })
            .expect("seed market should create a tracked submit order");

        {
            let manager_handle = services.runtime_lifecycle_service.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.mutation_frame("btc-core").unwrap();
            assert!(snapshot.set_binding_order_status_for_client_order_id(
                &tracked_request.client_order_id,
                Some("tracked-order".to_string()),
                BindingStatus::Working,
            ));
            snapshot.set_recovery_anomaly(Some(anomaly));
            manager.rollback_track_state(&snapshot).unwrap();
        }

        let exchange = OrderSetRecoveryExchange::new(
            tracked_request.instrument.clone(),
            build_open_orders(&tracked_request),
        );

        sync_exchange_state_from_exchange(
            &runtime_context,
            &exchange,
            "btc-core",
            &Instrument::new(Venue::Binance, "BTCUSDT"),
            ExchangeSyncMode::RecoverAndReconcile,
        )
        .await
        .unwrap();

        let summary = runtime_context
            .runtime_state()
            .reconcile
            .runtime_lifecycle_service
            .load_track_recovery_summary(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(summary.issue, None);
        assert_eq!(exchange.cancel_all_calls.load(Ordering::SeqCst), 1);
        assert_eq!(exchange.cancel_order_calls.load(Ordering::SeqCst), 0);
        assert!(exchange.open_orders_are_empty());
        assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 4);
        assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 2);
    }

    struct OrderSetRecoveryExchange {
        instrument: Instrument,
        open_orders: Mutex<Vec<ExchangeOrder>>,
        cancel_all_calls: AtomicUsize,
        cancel_order_calls: AtomicUsize,
        get_position_calls: AtomicUsize,
        get_open_orders_calls: AtomicUsize,
        clear_on_next_read: Mutex<bool>,
    }

    impl OrderSetRecoveryExchange {
        fn new(instrument: Instrument, open_orders: Vec<ExchangeOrder>) -> Self {
            Self {
                instrument,
                open_orders: Mutex::new(open_orders),
                cancel_all_calls: AtomicUsize::new(0),
                cancel_order_calls: AtomicUsize::new(0),
                get_position_calls: AtomicUsize::new(0),
                get_open_orders_calls: AtomicUsize::new(0),
                clear_on_next_read: Mutex::new(false),
            }
        }

        fn open_orders_are_empty(&self) -> bool {
            self.open_orders.lock().unwrap().is_empty()
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for OrderSetRecoveryExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Err(anyhow!("submit_order is not used in this test"))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> Result<OrderReceipt> {
            self.cancel_order_calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("cancel_order should not be used in this test"))
        }

        async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
            assert_eq!(instrument, &self.instrument);
            self.cancel_all_calls.fetch_add(1, Ordering::SeqCst);
            *self.clear_on_next_read.lock().unwrap() = true;
            Ok(())
        }

        async fn get_position(&self, instrument: &Instrument) -> Result<Position> {
            assert_eq!(instrument, &self.instrument);
            self.get_position_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Position {
                instrument: instrument.clone(),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(
            &self,
            instrument: &Instrument,
        ) -> Result<ExchangeOpenOrderSnapshot> {
            assert_eq!(instrument, &self.instrument);
            self.get_open_orders_calls.fetch_add(1, Ordering::SeqCst);
            let should_clear = {
                let mut clear_on_next_read = self.clear_on_next_read.lock().unwrap();
                if *clear_on_next_read {
                    *clear_on_next_read = false;
                    true
                } else {
                    false
                }
            };
            if should_clear {
                let snapshot = self.open_orders.lock().unwrap().clone();
                self.open_orders.lock().unwrap().clear();
                return Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                    snapshot,
                ));
            }
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                self.open_orders.lock().unwrap().clone(),
            ))
        }
    }
}
