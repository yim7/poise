use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use poise_application::{ApplicationNotification, TrackInstrument, TrackMutationError};
use poise_engine::manager::ExchangeSyncMode;
use poise_engine::ports::{ExchangeOrder, ExecutionPort};
use tokio::sync::watch;
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
        let mut notifications = state.notifications.subscribe();
        let mut pending_workset = RecoveryWorkset::default();
        let mut ticker = tokio::time::interval(Duration::from_millis(50));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

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
                notification = notifications.recv() => {
                    if !record_recovery_notification(&mut pending_workset, notification) {
                        break;
                    }
                    drain_recovery_notifications(&mut notifications, &mut pending_workset);
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

async fn load_recovery_anomaly_active(state: &ReconcileState, track_id: &str) -> Option<bool> {
    match state.mutation_store.load_track_state(track_id).await {
        Ok(Some(snapshot)) => Some(
            snapshot
                .executor_state
                .diagnostics
                .recovery_anomaly
                .is_some(),
        ),
        Ok(None) => Some(false),
        Err(error) => {
            tracing::warn!(
                "failed to load runtime snapshot for recovery tracking on `{track_id}`: {error}"
            );
            None
        }
    }
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
        tracing::warn!(
            "recovery notification stream lagged by {} messages; reseeding recovery tracking",
            workset.skipped_messages
        );
        *tracked = seed_recovery_tracking(state, instruments, retry_interval).await;
        return;
    }

    for track_id in &workset.changed_track_ids {
        if let Some(recovery_anomaly_active) =
            load_recovery_anomaly_active(state, track_id.as_str()).await
        {
            update_recovery_tracking(
                tracked,
                instruments,
                track_id.as_str(),
                recovery_anomaly_active,
                retry_interval,
            );
        }
    }
}

fn record_recovery_notification(
    workset: &mut RecoveryWorkset,
    notification: Result<ApplicationNotification, tokio::sync::broadcast::error::RecvError>,
) -> bool {
    match notification {
        Ok(ApplicationNotification::TrackChanged { track_id }) => {
            workset
                .changed_track_ids
                .insert(track_id.as_str().to_string());
            true
        }
        Ok(ApplicationNotification::AccountChanged) => true,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
            workset.reseed_required = true;
            workset.skipped_messages += skipped;
            true
        }
        Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
    }
}

fn drain_recovery_notifications(
    notifications: &mut tokio::sync::broadcast::Receiver<ApplicationNotification>,
    workset: &mut RecoveryWorkset,
) {
    loop {
        match notifications.try_recv() {
            Ok(notification) => {
                let _ = record_recovery_notification(workset, Ok(notification));
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                workset.reseed_required = true;
                workset.skipped_messages += skipped;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RecoveryWorkset {
    changed_track_ids: HashSet<String>,
    reseed_required: bool,
    skipped_messages: u64,
}

impl RecoveryWorkset {
    fn is_empty(&self) -> bool {
        self.changed_track_ids.is_empty() && !self.reseed_required && self.skipped_messages == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_workset_coalesces_track_marks() {
        let mut workset = RecoveryWorkset::default();

        assert!(record_recovery_notification(
            &mut workset,
            Ok(ApplicationNotification::TrackChanged {
                track_id: poise_engine::track::TrackId::new("BTCUSDT"),
            }),
        ));
        assert!(record_recovery_notification(
            &mut workset,
            Ok(ApplicationNotification::TrackChanged {
                track_id: poise_engine::track::TrackId::new("BTCUSDT"),
            }),
        ));
        assert!(record_recovery_notification(
            &mut workset,
            Ok(ApplicationNotification::TrackChanged {
                track_id: poise_engine::track::TrackId::new("ETHUSDT"),
            }),
        ));

        assert_eq!(
            workset,
            RecoveryWorkset {
                changed_track_ids: HashSet::from(["BTCUSDT".to_string(), "ETHUSDT".to_string()]),
                reseed_required: false,
                skipped_messages: 0,
            }
        );
    }

    #[test]
    fn recovery_workset_coalesces_reseed_requests() {
        let mut workset = RecoveryWorkset::default();
        assert!(record_recovery_notification(
            &mut workset,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(1)),
        ));
        assert!(record_recovery_notification(
            &mut workset,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(2)),
        ));
        assert!(record_recovery_notification(
            &mut workset,
            Ok(ApplicationNotification::TrackChanged {
                track_id: poise_engine::track::TrackId::new("SOLUSDT"),
            }),
        ));

        assert_eq!(
            workset,
            RecoveryWorkset {
                changed_track_ids: HashSet::from(["SOLUSDT".to_string()]),
                reseed_required: true,
                skipped_messages: 3,
            }
        );
    }
}
