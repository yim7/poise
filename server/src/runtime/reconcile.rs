use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use poise_application::{RecoveryAnomalyObserver, TrackInstrument, TrackMutationError};
use poise_engine::manager::ExchangeSyncMode;
use poise_engine::ports::{ExchangeOrder, ExecutionPort};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::order_outcome::reconcile_execution;
use crate::server_context::ReconcileState;

use super::{
    ReconcileExecution, ReconcileRequest, ReconcileStateAccess, ServerRuntime, order_observation,
    position_observation, preserve_track_mutation_error,
};

#[derive(Debug, Clone)]
struct RecoveryTrackedTrack {
    instrument: poise_engine::track::Instrument,
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
        track_id: &poise_engine::track::TrackId,
        active: bool,
    ) {
        self.workset
            .lock()
            .unwrap()
            .anomaly_updates
            .insert(track_id.as_str().to_string(), active);
        self.notify.notify_one();
    }

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
    fn observe_recovery_anomaly_change(
        &self,
        track_id: &poise_engine::track::TrackId,
        active: bool,
    ) {
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
                    for track in &instruments {
                        if let Err(error) = state
                            .reconcile
                            .observation_service
                            .refresh_market_data_health(&track.id)
                            .await
                        {
                            tracing::warn!(
                                "failed to refresh market data health for {}: {}",
                                track.instrument.symbol,
                                error
                            );
                        }
                    }

                    let now = Instant::now();
                    let due_anomaly_tracks: Vec<(String, poise_engine::track::Instrument)> = tracked
                        .iter()
                        .filter(|(_, tracked_track)| tracked_track.next_retry_at <= now)
                        .map(|(track_id, tracked_track)| (track_id.clone(), tracked_track.instrument.clone()))
                        .collect();
                    let due_audit_tracks: Vec<(String, poise_engine::track::Instrument)> = instruments
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
    instrument: &poise_engine::track::Instrument,
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
    instrument: &poise_engine::track::Instrument,
    mode: ExchangeSyncMode,
) -> std::result::Result<(), TrackMutationError> {
    let state = state.reconcile_state_view();
    let _reconcile_guard = state.reconcile_guards.lock(track_id).await;
    let sync_token = state.exchange_freshness.prepare_sync(track_id).await;
    let snapshot = state
        .mutation_store
        .load_track_state(track_id)
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

    if should_cancel_unknown_live_orders(snapshot.as_ref(), &open_orders) {
        for order in &open_orders {
            execution
                .cancel_order(instrument, &order.order_id)
                .await
                .with_context(|| {
                    format!(
                        "failed to cancel unknown live order `{}` for {}",
                        order.order_id, instrument.symbol
                    )
                })
                .map_err(TrackMutationError::Persistence)?;
        }
        position = execution
            .get_position(instrument)
            .await
            .map_err(TrackMutationError::Persistence)?;
        open_orders = execution
            .get_open_orders(instrument)
            .await
            .map_err(TrackMutationError::Persistence)?;
    }

    if matches!(mode, ExchangeSyncMode::RecoverAndReconcile) {
        let _ = state
            .observation_service
            .sync_exchange_state(
                track_id,
                position_observation(&position),
                open_orders.iter().map(order_observation).collect(),
            )
            .await
            .map_err(preserve_track_mutation_error)?;
    } else {
        let _ = state
            .observation_service
            .sync_exchange_state_without_reconcile(
                track_id,
                position_observation(&position),
                open_orders.iter().map(order_observation).collect(),
            )
            .await
            .map_err(preserve_track_mutation_error)?;
    }
    state.exchange_freshness.clear_if_current(sync_token).await;
    Ok(())
}

fn should_cancel_unknown_live_orders(
    snapshot: Option<&poise_engine::snapshot::TrackRuntimeSnapshot>,
    open_orders: &[ExchangeOrder],
) -> bool {
    !open_orders.is_empty()
        && snapshot.is_some_and(|snapshot| {
            snapshot.executor_state.diagnostics.recovery_anomaly
                == Some(poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
                && snapshot
                    .executor_state
                    .slots
                    .iter()
                    .all(|slot| slot.working_order.is_none())
        })
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
        let Ok(Some(snapshot)) = state.mutation_store.load_track_state(&track.id).await else {
            continue;
        };
        update_recovery_tracking(
            &mut tracked,
            instruments,
            &track.id,
            snapshot
                .executor_state
                .diagnostics
                .recovery_anomaly
                .is_some(),
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
    use super::*;

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
        let mut workset = RecoveryWorkset::default();
        workset.reseed_required = true;
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
}
