use anyhow::Result;
use poise_application::{CancelReceiptResolution, SessionEffectOutcome, SessionTrackEffect};
use poise_engine::transition::TrackEffect;

use super::{Cancellation, EffectWorker};

pub(super) async fn run_once(worker: &EffectWorker) -> Result<()> {
    loop {
        if *worker.shutdown_rx.borrow() {
            break;
        }

        let Some(effect) = worker.state.session_effect_queue.claim_next() else {
            break;
        };
        let effect_id = effect.effect_id.clone();
        let outcome = match worker.process_effect(effect).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::warn!("failed to process session effect: {error}");
                SessionDispatchResult::Outcome(SessionEffectOutcome::Blocked {
                    reason: error.to_string(),
                })
            }
        };
        match outcome {
            SessionDispatchResult::Outcome(outcome) => {
                worker
                    .state
                    .session_effect_queue
                    .record_outcome(&effect_id, outcome);
            }
            SessionDispatchResult::Cancel(resolution) => {
                worker
                    .state
                    .session_effect_queue
                    .record_cancel_resolution(&effect_id, resolution);
            }
        }
    }

    Ok(())
}

pub(super) enum SessionDispatchResult {
    Outcome(SessionEffectOutcome),
    Cancel(CancelReceiptResolution),
}

pub(super) async fn process_effect(
    worker: &EffectWorker,
    effect: SessionTrackEffect,
) -> Result<SessionDispatchResult> {
    match &effect.effect {
        TrackEffect::SubmitOrder {
            request,
            desired_exposure,
            recovery_token,
            ..
        } => worker
            .execute_submit(
                &effect,
                request.clone(),
                recovery_token.clone(),
                desired_exposure.clone(),
            )
            .await
            .map(SessionDispatchResult::Outcome),
        TrackEffect::CancelOrder {
            instrument,
            order_id,
        } => {
            worker
                .execute_cancellation(
                    &effect,
                    Cancellation::One {
                        instrument: instrument.clone(),
                        order_id: order_id.clone(),
                    },
                )
                .await
        }
        TrackEffect::CancelAll { instrument } => {
            worker
                .execute_cancellation(
                    &effect,
                    Cancellation::All {
                        instrument: instrument.clone(),
                    },
                )
                .await
        }
        TrackEffect::NoOp => {
            worker
                .state
                .effect_service
                .complete_effect_succeeded(effect.track_id.as_str(), &effect.effect_id)
                .await?;
            Ok(SessionDispatchResult::Outcome(
                SessionEffectOutcome::Finished,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Utc;
    use poise_application::SessionTrackEffect;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use poise_storage::sqlite::SqliteStorage;

    use crate::effect_worker::EffectWorker;
    use crate::test_support::{
        NoopAccountPort, RecordingExecutionPort, build_effect_worker_context_for_repository,
        seed_persisted_pending_submit_effect,
    };

    #[tokio::test]
    async fn worker_does_not_dispatch_persisted_effects_from_previous_session() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        seed_persisted_pending_submit_effect(repository.as_ref(), "btc-core")
            .await
            .unwrap();

        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        let execution = Arc::new(RecordingExecutionPort::default());
        let account = Arc::new(NoopAccountPort);
        let worker = EffectWorker::new(
            effect_worker_context,
            execution.clone(),
            account,
            Duration::from_millis(1),
        );

        worker.run_once().await.unwrap();

        assert_eq!(
            execution.submit_order_call_count(),
            0,
            "effect worker must not dispatch persisted pending effects from a previous session"
        );
    }

    #[tokio::test]
    async fn worker_dispatches_current_session_queue_effect() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .enqueue_batch(vec![SessionTrackEffect {
                effect_id: "session-effect-1".to_string(),
                track_id: TrackId::new("btc-core"),
                batch_id: "session-batch-1".to_string(),
                sequence: 0,
                effect: TrackEffect::CancelAll {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                },
                created_at: Utc::now(),
            }]);

        let execution = Arc::new(RecordingExecutionPort::default());
        let account = Arc::new(NoopAccountPort);
        let worker = EffectWorker::new(
            effect_worker_context,
            execution.clone(),
            account,
            Duration::from_millis(1),
        );

        worker.run_once().await.unwrap();

        assert_eq!(execution.cancel_all_call_count(), 1);
    }
}
