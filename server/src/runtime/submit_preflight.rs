use poise_application::ApplicationNotification;
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
    let current_pending_submit_effect_ids = state.session_effect_queue.active_submit_effect_ids();
    state
        .submit_preflight
        .reconcile_pending_submit_effects(&current_pending_submit_effect_ids)
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use poise_storage::sqlite::SqliteStorage;

    use crate::submit_preflight::SubmitPreflightDecision;
    use crate::test_support::{
        build_effect_worker_context_for_repository, seed_persisted_pending_submit_effect,
    };

    use super::reconcile_submit_preflight_state;

    #[tokio::test]
    async fn reconcile_ignores_persisted_effects_from_previous_session() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let stale_effect_id = seed_persisted_pending_submit_effect(repository.as_ref(), "btc-core")
            .await
            .unwrap();
        let context = build_effect_worker_context_for_repository(repository);

        context
            .submit_preflight
            .mark_submit_started(&stale_effect_id)
            .await;
        reconcile_submit_preflight_state(&context.effect_worker_state.reconcile)
            .await
            .unwrap();

        assert_eq!(
            context
                .submit_preflight
                .decide(&stale_effect_id, "old-session-client")
                .await,
            SubmitPreflightDecision::Direct,
            "previous-session persisted pending effects must not keep current-session preflight state alive"
        );
    }
}
