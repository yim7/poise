use std::collections::{HashMap, HashSet};
use std::future::Future;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use poise_engine::ports::{AccountCapacitySnapshot, UserDataEvent};
use poise_engine::track::Instrument;
use tokio::sync::mpsc;
use tokio::time::sleep;

use super::{
    RuntimeStartupCapacityMode, RuntimeStartupDefinition, STARTUP_RETRY_ATTEMPTS,
    STARTUP_RETRY_DELAY, ServerRuntime, exchange_state,
};

struct TrackStartupSeed {
    track_id: String,
    position: poise_engine::observation::PositionObservation,
    open_orders: Vec<poise_engine::observation::OrderObservation>,
    account_capacity_snapshot: AccountCapacitySnapshot,
    cleanup_filter: CleanupReplayFilter,
}

#[derive(Debug, Clone, Default)]
struct CleanupReplayFilter {
    order_ids: HashSet<String>,
    client_order_ids: HashSet<String>,
}

impl CleanupReplayFilter {
    fn from_orders(orders: &[poise_engine::ports::ExchangeOrder]) -> Self {
        Self {
            order_ids: orders.iter().map(|order| order.order_id.clone()).collect(),
            client_order_ids: orders
                .iter()
                .map(|order| order.client_order_id.clone())
                .collect(),
        }
    }

    fn matches_order(&self, order: &poise_engine::ports::ExchangeOrder) -> bool {
        self.order_ids.contains(order.order_id.as_str())
            || self
                .client_order_ids
                .contains(order.client_order_id.as_str())
    }
}

pub(super) async fn complete_startup(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    startup_cutoff: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let mut account_capacity_snapshots: HashMap<Instrument, AccountCapacitySnapshot> =
        HashMap::new();
    let mut track_seeds = Vec::new();

    for track in &runtime.startup_definitions {
        let instrument = track.instrument().clone();
        let seed = prepare_track_startup_seed(runtime, track).await?;
        account_capacity_snapshots.insert(instrument, seed.account_capacity_snapshot.clone());
        track_seeds.push(seed);
    }

    runtime
        .state
        .account_margin_guard
        .replace_snapshots(account_capacity_snapshots);

    let mut cleanup_filters = HashMap::new();
    for seed in track_seeds {
        cleanup_filters.insert(seed.track_id.clone(), seed.cleanup_filter.clone());
        runtime
            .state
            .reconcile
            .runtime_lifecycle_service
            .prepare_fresh_session_for_activation(&seed.track_id)
            .await?;
        runtime
            .state
            .reconcile
            .observation_service
            .sync_exchange_state(&seed.track_id, seed.position, seed.open_orders)
            .await?;
    }

    replay_startup_user_data(runtime, receiver, startup_cutoff, &cleanup_filters).await?;
    retry_startup_step("get_server_time", || runtime.metadata.get_server_time()).await
}

async fn prepare_track_startup_seed(
    runtime: &ServerRuntime,
    track: &RuntimeStartupDefinition,
) -> Result<TrackStartupSeed> {
    let instrument = track.instrument().clone();
    let cleanup_filter = clear_inherited_open_orders(runtime, &instrument).await?;
    let position = retry_startup_step("get_position", || {
        runtime.execution.get_position(&instrument)
    })
    .await?;
    let account_capacity_snapshot = probe_startup_account_capacity(runtime, track).await?;

    let required_additional_notional = track.required_additional_notional(position.qty);
    if required_additional_notional > account_capacity_snapshot.max_increase_notional {
        return Err(anyhow!(
            "insufficient account margin for configured max_notional on track `{}`: required {}, available {}",
            track.track_id().as_str(),
            required_additional_notional,
            account_capacity_snapshot.max_increase_notional
        ));
    }

    Ok(TrackStartupSeed {
        track_id: track.track_id().as_str().to_string(),
        position: exchange_state::position_observation(&position),
        open_orders: Vec::new(),
        account_capacity_snapshot,
        cleanup_filter,
    })
}

async fn clear_inherited_open_orders(
    runtime: &ServerRuntime,
    instrument: &Instrument,
) -> Result<CleanupReplayFilter> {
    let open_orders = retry_startup_step("get_open_orders", || {
        runtime.execution.get_open_orders(instrument)
    })
    .await?;
    if open_orders.is_empty() {
        return Ok(CleanupReplayFilter::default());
    }
    let cleanup_filter = CleanupReplayFilter::from_orders(&open_orders);

    retry_startup_step("cancel_all", || runtime.execution.cancel_all(instrument)).await?;
    retry_startup_step("await_open_orders_cleared", || async {
        let remaining = runtime.execution.get_open_orders(instrument).await?;
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "inherited open orders still present for {}:{} after cancel_all",
                instrument.venue.as_str(),
                instrument.symbol
            ))
        }
    })
    .await?;

    Ok(cleanup_filter)
}

async fn probe_startup_account_capacity(
    runtime: &ServerRuntime,
    track: &RuntimeStartupDefinition,
) -> Result<AccountCapacitySnapshot> {
    match track.startup_capacity_mode() {
        RuntimeStartupCapacityMode::AvailableBalanceTimesLeverage { leverage } => {
            let summary = retry_startup_step("get_account_summary", || {
                runtime.account_summary.get_account_summary()
            })
            .await?;
            Ok(AccountCapacitySnapshot {
                max_increase_notional: summary.available * *leverage as f64,
            })
        }
        RuntimeStartupCapacityMode::AccountCapacitySnapshot => {
            let instrument = track.instrument().clone();
            retry_startup_step("get_account_capacity_snapshot", || {
                runtime.account.get_account_capacity_snapshot(&instrument)
            })
            .await
        }
    }
}

async fn replay_startup_user_data(
    runtime: &ServerRuntime,
    receiver: &mut mpsc::Receiver<UserDataEvent>,
    startup_cutoff: DateTime<Utc>,
    cleanup_filters: &HashMap<String, CleanupReplayFilter>,
) -> Result<()> {
    let mut buffered_events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        buffered_events.push(event);
    }

    buffered_events.sort_by_key(|event| event.event_time);
    for event in buffered_events {
        if event.event_time > startup_cutoff {
            let instrument = event.instrument().clone();
            let Some(track_id) = runtime
                .state
                .reconcile
                .observation_service
                .resolve_track_id(&instrument)
                .await
            else {
                tracing::warn!(
                    "received user data for unknown instrument {}:{}",
                    instrument.venue.as_str(),
                    instrument.symbol
                );
                continue;
            };
            if should_ignore_cleanup_event(cleanup_filters, &track_id, &event) {
                continue;
            }
            exchange_state::apply_user_data_event(
                &runtime.state.reconcile,
                runtime.execution.as_ref(),
                &track_id,
                event,
            )
            .await
            .map_err(super::mutate_error)?;
        }
    }

    Ok(())
}

fn should_ignore_cleanup_event(
    cleanup_filters: &HashMap<String, CleanupReplayFilter>,
    track_id: &str,
    event: &UserDataEvent,
) -> bool {
    let Some(filter) = cleanup_filters.get(track_id) else {
        return false;
    };

    match &event.payload {
        poise_engine::ports::UserDataPayload::OrderUpdate(order) => filter.matches_order(order),
        _ => false,
    }
}

pub(super) async fn retry_startup_step<T, F, Fut>(
    step_name: &'static str,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt + 1 == STARTUP_RETRY_ATTEMPTS {
                    return Err(error);
                }
                tracing::warn!(
                    step = step_name,
                    attempt = attempt + 1,
                    max_attempts = STARTUP_RETRY_ATTEMPTS,
                    "startup step failed: {error}"
                );
                last_error = Some(error);
            }
        }

        sleep(STARTUP_RETRY_DELAY).await;
    }

    Err(last_error.unwrap_or_else(|| anyhow!("startup step `{step_name}` failed")))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use poise_application::{
        EffectStatus, TrackEffectStore, TrackMutationStore, TrackQueryStore,
    };
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        AccountPort, AccountSummaryPort, ExchangeInfo, ExchangeOrder, ExecutionPort,
        MetadataPort, OrderRequest, OrderStatus, Position, PriceTick, UserDataEvent,
        UserDataPayload,
    };
    use poise_engine::price_gate::SubmitPurpose;
    use poise_engine::runtime::TrackStatus;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use crate::assembly::SystemClock;
    use crate::runtime::{RuntimePorts, RuntimeStartupCapacityMode, RuntimeStartupDefinition};
    use crate::test_support::{
        build_runtime_and_effect_worker_test_contexts, build_test_application_services,
        test_prepared_registry, unavailable_account_monitor,
    };

    use super::complete_startup;

    #[tokio::test]
    async fn complete_startup_cancels_inherited_orders_and_rebuilds_fresh_executor_state() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = seeded_manager_with_active_binding();
        let snapshot = manager.snapshot("btc-core").unwrap();
        repository
            .save_transition(
                "btc-core",
                &snapshot,
                &[],
                &[
                    pending_submit_effect("BTCUSDT", "boundary-catch-up-legacy"),
                    TrackEffect::CancelAll {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    },
                ],
            )
            .await
            .unwrap();
        repository
            .save_follow_up_retirement_request(
                &TrackId::new("btc-core"),
                &poise_application::FollowUpRetirementRequest {
                    batch_id: "btc-core:batch".into(),
                    blocked_sequence: 0,
                    closed_order_id: "legacy-order".into(),
                },
            )
            .await
            .unwrap();

        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectStore>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications.clone());
        let (runtime_context, effect_worker_context) = build_runtime_and_effect_worker_test_contexts(
            &services,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectStore>,
            account_monitor,
        );
        let exchange = Arc::new(StartupExchange::with_inherited_order("BTCUSDT"));
        let runtime = super::ServerRuntime::new(
            runtime_context.runtime_state(),
            effect_worker_context.effect_worker_state,
            RuntimePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                Arc::new(SystemClock),
            ),
            vec![RuntimeStartupDefinition::new(
                test_prepared_registry("btc-core")
                    .get(&TrackId::new("btc-core"))
                    .unwrap()
                    .startup_definition(),
                RuntimeStartupCapacityMode::AccountCapacitySnapshot,
            )],
        );
        let (_sender, mut receiver) = mpsc::channel(8);

        let _steady_state_cutoff = complete_startup(&runtime, &mut receiver, Utc::now())
            .await
            .unwrap();

        assert_eq!(exchange.cancel_all_calls.load(Ordering::SeqCst), 1);
        assert!(
            !runtime_context
                .submit_preflight
                .has_tracked_submit_effects()
                .await
        );
        assert!(repository.list_all_pending_submit_effects().await.unwrap().is_empty());
        assert!(repository.list_dispatchable_effects().await.unwrap().is_empty());
        assert!(
            repository
                .list_follow_up_retirement_requests(&TrackId::new("btc-core"))
                .await
                .unwrap()
                .is_empty()
        );

        let effects = repository
            .list_recent_track_effects(&TrackId::new("btc-core"), 8)
            .await
            .unwrap();
        assert_eq!(
            effects
                .iter()
                .filter(|effect| effect.status == EffectStatus::Superseded)
                .count(),
            2
        );

        let snapshot = services
            .observation_service
            .manager()
            .read()
            .await
            .snapshot("btc-core")
            .unwrap();
        assert!(snapshot.executor_state.bindings.is_empty());
        assert!(snapshot.executor_state.recovery_anomaly.is_none());
        assert_eq!(snapshot.status(), TrackStatus::WaitingMarketData);
    }

    #[tokio::test]
    async fn complete_startup_ignores_cleanup_order_updates_but_replays_new_session_events() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = seeded_manager_with_active_binding();
        let snapshot = manager.snapshot("btc-core").unwrap();
        repository
            .save_transition("btc-core", &snapshot, &[], &[])
            .await
            .unwrap();

        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectStore>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications.clone());
        let (runtime_context, effect_worker_context) = build_runtime_and_effect_worker_test_contexts(
            &services,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectStore>,
            account_monitor,
        );
        let exchange = Arc::new(StartupExchange::with_inherited_order("BTCUSDT"));
        let runtime = super::ServerRuntime::new(
            runtime_context.runtime_state(),
            effect_worker_context.effect_worker_state,
            RuntimePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                Arc::new(SystemClock),
            ),
            vec![RuntimeStartupDefinition::new(
                test_prepared_registry("btc-core")
                    .get(&TrackId::new("btc-core"))
                    .unwrap()
                    .startup_definition(),
                RuntimeStartupCapacityMode::AccountCapacitySnapshot,
            )],
        );
        let (sender, mut receiver) = mpsc::channel(8);
        let startup_cutoff = Utc.with_ymd_and_hms(2026, 4, 23, 12, 0, 0).unwrap();

        sender
            .send(UserDataEvent {
                event_time: startup_cutoff + chrono::TimeDelta::seconds(1),
                payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    order_id: "legacy-order".into(),
                    client_order_id: "legacy-client-order".into(),
                    side: Side::Buy,
                    price: 99.0,
                    qty: 0.1,
                    filled_qty: 0.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::Canceled,
                }),
            })
            .await
            .unwrap();
        sender
            .send(UserDataEvent {
                event_time: startup_cutoff + chrono::TimeDelta::seconds(2),
                payload: UserDataPayload::PositionUpdate(Position {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    qty: 1.25,
                    avg_price: 100.0,
                    unrealized_pnl: 12.0,
                }),
            })
            .await
            .unwrap();
        drop(sender);

        let _steady_state_cutoff = complete_startup(&runtime, &mut receiver, startup_cutoff)
            .await
            .unwrap();

        assert!(!runtime_context.exchange_freshness.is_stale("btc-core").await);
        assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 2);
        assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 1);
        let snapshot = services
            .observation_service
            .manager()
            .read()
            .await
            .snapshot("btc-core")
            .unwrap();
        assert_eq!(snapshot.current_exposure, Exposure(0.3333333333333333));
        assert_eq!(snapshot.risk.unrealized_pnl, 12.0);
    }

    #[tokio::test]
    async fn user_task_uses_steady_state_cutoff_after_startup_cleanup_phase() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let manager = seeded_manager_with_active_binding();
        let snapshot = manager.snapshot("btc-core").unwrap();
        repository
            .save_transition("btc-core", &snapshot, &[], &[])
            .await
            .unwrap();

        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectStore>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications.clone());
        let (runtime_context, effect_worker_context) = build_runtime_and_effect_worker_test_contexts(
            &services,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectStore>,
            account_monitor,
        );
        let exchange = Arc::new(StartupExchange::with_inherited_order("BTCUSDT"));
        let runtime = super::ServerRuntime::new(
            runtime_context.runtime_state(),
            effect_worker_context.effect_worker_state,
            RuntimePorts::new(
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
                Arc::new(SystemClock),
            ),
            vec![RuntimeStartupDefinition::new(
                test_prepared_registry("btc-core")
                    .get(&TrackId::new("btc-core"))
                    .unwrap()
                    .startup_definition(),
                RuntimeStartupCapacityMode::AccountCapacitySnapshot,
            )],
        );
        let (sender, mut receiver) = mpsc::channel(8);
        let startup_cutoff = Utc.with_ymd_and_hms(2026, 4, 23, 12, 0, 0).unwrap();

        let steady_state_cutoff = complete_startup(&runtime, &mut receiver, startup_cutoff)
            .await
            .unwrap();
        let user_task =
            runtime.spawn_user_task(receiver, steady_state_cutoff, runtime.shutdown_tx.subscribe());

        sender
            .send(UserDataEvent {
                event_time: steady_state_cutoff - chrono::TimeDelta::milliseconds(1),
                payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    order_id: "legacy-order".into(),
                    client_order_id: "legacy-client-order".into(),
                    side: Side::Buy,
                    price: 99.0,
                    qty: 0.1,
                    filled_qty: 0.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::Canceled,
                }),
            })
            .await
            .unwrap();
        sender
            .send(UserDataEvent {
                event_time: steady_state_cutoff + chrono::TimeDelta::seconds(1),
                payload: UserDataPayload::PositionUpdate(Position {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    qty: 1.25,
                    avg_price: 100.0,
                    unrealized_pnl: 12.0,
                }),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let snapshot = services
                    .observation_service
                    .manager()
                    .read()
                    .await
                    .snapshot("btc-core")
                    .unwrap();
                if snapshot.current_exposure == Exposure(0.3333333333333333)
                    && snapshot.risk.unrealized_pnl == 12.0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 2);
        assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 1);
        assert!(!runtime_context.exchange_freshness.is_stale("btc-core").await);

        runtime.shutdown_tx.send(true).unwrap();
        user_task.await.unwrap();
    }

    fn seeded_manager_with_active_binding() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(SystemClock));
        manager
            .add_track(
                TrackId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                TrackConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: 0.5,
                    shape_family: ShapeFamily::Linear,
                    out_of_band_policy: BandProtectionPolicy::Freeze,
                },
                3_000.0,
                LossLimits {
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                },
                ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.001,
                    min_qty: 0.001,
                    min_notional: 5.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            )
            .unwrap();
        manager
            .record_submit_request(
                &TrackId::new("btc-core"),
                &OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    side: Side::Buy,
                    price: 100.0,
                    quantity: 0.1,
                    client_order_id: "legacy-binding".into(),
                    reduce_only: false,
                },
                Exposure(4.0),
            )
            .unwrap();
        manager
    }

    fn pending_submit_effect(symbol: &str, client_order_id: &str) -> TrackEffect {
        TrackEffect::SubmitOrder {
            request: OrderRequest {
                instrument: Instrument::new(Venue::Binance, symbol),
                side: Side::Buy,
                price: 100.0,
                quantity: 0.1,
                client_order_id: client_order_id.to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(4.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            recovery_token: poise_engine::executor::SubmitRecoveryToken::empty(),
        }
    }

    struct StartupExchange {
        inherited_order_present: AtomicBool,
        cancel_all_calls: AtomicUsize,
        get_position_calls: AtomicUsize,
        get_open_orders_calls: AtomicUsize,
        instrument: Instrument,
    }

    impl StartupExchange {
        fn with_inherited_order(symbol: &str) -> Self {
            Self {
                inherited_order_present: AtomicBool::new(true),
                cancel_all_calls: AtomicUsize::new(0),
                get_position_calls: AtomicUsize::new(0),
                get_open_orders_calls: AtomicUsize::new(0),
                instrument: Instrument::new(Venue::Binance, symbol),
            }
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for StartupExchange {
        async fn submit_order(
            &self,
            _req: poise_engine::ports::OrderRequest,
        ) -> Result<poise_engine::ports::OrderReceipt> {
            Err(anyhow!("submit_order is not used during startup bootstrap tests"))
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Err(anyhow!("cancel_order is not used during startup bootstrap tests"))
        }

        async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
            assert_eq!(instrument, &self.instrument);
            self.cancel_all_calls.fetch_add(1, Ordering::SeqCst);
            self.inherited_order_present.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn get_position(&self, instrument: &Instrument) -> Result<Position> {
            assert_eq!(instrument, &self.instrument);
            self.get_position_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Position {
                instrument: instrument.clone(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(&self, instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            assert_eq!(instrument, &self.instrument);
            self.get_open_orders_calls.fetch_add(1, Ordering::SeqCst);
            if !self.inherited_order_present.load(Ordering::SeqCst) {
                return Ok(Vec::new());
            }

            Ok(vec![ExchangeOrder {
                instrument: instrument.clone(),
                order_id: "legacy-order".into(),
                client_order_id: "legacy-client-order".into(),
                side: Side::Buy,
                price: 99.0,
                qty: 0.1,
                filled_qty: 0.0,
                realized_pnl: 0.0,
                status: OrderStatus::New,
            }])
        }
    }

    #[async_trait::async_trait]
    impl AccountSummaryPort for StartupExchange {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: Utc::now(),
            })
        }
    }

    #[async_trait::async_trait]
    impl AccountPort for StartupExchange {
        async fn get_account_capacity_snapshot(
            &self,
            instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
            assert_eq!(instrument, &self.instrument);
            Ok(poise_engine::ports::AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<poise_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    #[async_trait::async_trait]
    impl MetadataPort for StartupExchange {
        async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
            assert_eq!(instrument, &self.instrument);
            Ok(ExchangeInfo {
                instrument: instrument.clone(),
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.001,
                    min_qty: 0.001,
                    min_notional: 5.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::MarketDataPort for StartupExchange {
        async fn subscribe_prices(&self, _instrument: &Instrument) -> Result<mpsc::Receiver<PriceTick>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
