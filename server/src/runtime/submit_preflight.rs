use poise_application::ApplicationNotification;
use std::collections::HashSet;
use std::time::Duration;

use poise_application::TrackMutationError;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{ReconcileStateAccess, ServerRuntime};

pub(super) fn spawn_submit_preflight_task(
    runtime: &ServerRuntime,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.clone();

    tokio::spawn(async move {
        let mut notifications = state.notifications.subscribe();
        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            if !state
                .reconcile
                .submit_preflight
                .take_pending_submit_effects_dirty()
            {
                tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    notification = notifications.recv() => {
                        match notification {
                            Ok(ApplicationNotification::TrackChanged { .. })
                            | Ok(ApplicationNotification::AccountChanged)
                            | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                if state
                                    .reconcile
                                    .submit_preflight
                                    .has_tracked_submit_effects()
                                    .await
                                {
                                    state
                                        .reconcile
                                        .submit_preflight
                                        .mark_pending_submit_effects_dirty();
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = state
                        .reconcile
                        .submit_preflight
                        .wait_for_pending_submit_effects_dirty() => {}
                }

                if *shutdown_rx.borrow() {
                    break;
                }

                if !state
                    .reconcile
                    .submit_preflight
                    .take_pending_submit_effects_dirty()
                {
                    continue;
                }
            }

            if let Err(error) = reconcile_submit_preflight_state(&state.reconcile).await {
                tracing::warn!(
                    "failed to reconcile submit preflight state after pending effect change: {}",
                    error.message()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
                state
                    .reconcile
                    .submit_preflight
                    .mark_pending_submit_effects_dirty();
            }
        }
    })
}

pub(super) async fn reconcile_submit_preflight_state(
    state: &impl ReconcileStateAccess,
) -> std::result::Result<(), TrackMutationError> {
    let state = state.reconcile_state_view();
    let current_pending_submit_effect_ids: HashSet<String> = state
        .effect_store
        .list_all_pending_submit_effects()
        .await
        .map_err(TrackMutationError::Persistence)?
        .into_iter()
        .map(|effect| effect.effect_id)
        .collect();
    state
        .submit_preflight
        .reconcile_pending_submit_effects(&current_pending_submit_effect_ids)
        .await;
    Ok(())
}
