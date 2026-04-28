use anyhow::{Result, anyhow};
use poise_application::{
    CancelQueueAction, CancelReceiptResolution, SessionEffectOutcome, SessionQueueAction,
    SessionTrackEffect,
};
use poise_engine::execution_plan::TrackEffect;
use poise_engine::track::{Instrument, TrackId};

use super::{Cancellation, EffectWorker};
use crate::order_outcome::{ReconcileReason, cancel_writeback_outcome_unknown};

pub(super) async fn run_once(worker: &EffectWorker) -> Result<()> {
    loop {
        if *worker.shutdown_rx.borrow() {
            break;
        }

        let Some(effect) = worker.state.session_effect_queue.claim_next() else {
            break;
        };
        let effect_id = effect.effect_id.clone();
        let track_id = effect.track_id.clone();
        let instrument = effect_instrument(&effect.effect).cloned();
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
                let action = worker
                    .state
                    .session_effect_queue
                    .record_outcome(&effect_id, outcome);
                handle_session_queue_action(worker, &track_id, instrument.as_ref(), action).await?;
            }
            SessionDispatchResult::Cancel(resolution) => {
                let action = worker
                    .state
                    .session_effect_queue
                    .record_cancel_resolution(&effect_id, resolution);
                handle_cancel_queue_action(worker, &track_id, instrument.as_ref(), action).await?;
            }
        }
    }

    Ok(())
}

async fn handle_session_queue_action(
    worker: &EffectWorker,
    track_id: &TrackId,
    instrument: Option<&Instrument>,
    action: SessionQueueAction,
) -> Result<()> {
    match action {
        SessionQueueAction::RetiredBatch {
            effect_ids,
            requires_reconcile: true,
        } => {
            record_superseded_effects(worker, track_id, &effect_ids).await?;
            trigger_queue_reconcile(
                worker,
                track_id,
                instrument,
                ReconcileReason::ManualRecovery,
            )
            .await?
        }
        SessionQueueAction::RetiredBatch {
            effect_ids,
            requires_reconcile: false,
        } => {
            record_superseded_effects(worker, track_id, &effect_ids).await?;
        }
        SessionQueueAction::Continue => {}
    }
    Ok(())
}

async fn handle_cancel_queue_action(
    worker: &EffectWorker,
    track_id: &TrackId,
    instrument: Option<&Instrument>,
    action: CancelQueueAction,
) -> Result<()> {
    match action {
        CancelQueueAction::SupersededDownstream {
            effect_ids,
            requires_reconcile: true,
        } => {
            record_superseded_effects(worker, track_id, &effect_ids).await?;
            trigger_queue_reconcile(
                worker,
                track_id,
                instrument,
                ReconcileReason::ManualRecovery,
            )
            .await?
        }
        CancelQueueAction::SupersededDownstream {
            effect_ids,
            requires_reconcile: false,
        } => {
            record_superseded_effects(worker, track_id, &effect_ids).await?;
        }
        CancelQueueAction::AwaitingCancelFollowUp { .. } => {
            if let Some(instrument) = instrument {
                worker
                    .recover_unknown_outcome(
                        track_id.as_str(),
                        instrument,
                        cancel_writeback_outcome_unknown(),
                    )
                    .await?;
            }
        }
        CancelQueueAction::UnblockedDownstream | CancelQueueAction::Deferred { .. } => {}
        CancelQueueAction::Blocked { reason } => {
            return Err(anyhow!(
                "cancel queue action blocked for track `{}`: {reason}",
                track_id.as_str()
            ));
        }
    }
    Ok(())
}

async fn record_superseded_effects(
    worker: &EffectWorker,
    track_id: &TrackId,
    effect_ids: &[String],
) -> Result<()> {
    worker
        .state
        .effect_service
        .record_effects_superseded(track_id.as_str(), effect_ids)
        .await
}

async fn trigger_queue_reconcile(
    worker: &EffectWorker,
    track_id: &TrackId,
    instrument: Option<&Instrument>,
    reason: ReconcileReason,
) -> Result<()> {
    if let Some(instrument) = instrument {
        worker
            .trigger_immediate_reconcile(track_id.as_str(), instrument, reason)
            .await?;
    }
    Ok(())
}

fn effect_instrument(effect: &TrackEffect) -> Option<&Instrument> {
    match effect {
        TrackEffect::SubmitOrder { request, .. } => Some(&request.instrument),
        TrackEffect::CancelOrder { instrument, .. } | TrackEffect::CancelAll { instrument } => {
            Some(instrument)
        }
        TrackEffect::NoOp => None,
    }
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

    use anyhow::Result;
    use chrono::Utc;
    use poise_core::types::{Exposure, Side};
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::ports::{
        ExchangeOpenOrderSnapshot, ExecutionPort, OrderReceipt, OrderRequest, Position,
    };
    use poise_engine::price_gate::SubmitPurpose;
    use poise_engine::track::{Instrument, TrackId, Venue};
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
        let track_id = TrackId::new("btc-core");
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .enqueue_transition_effects_for_test(
                &track_id,
                &[TrackEffect::CancelAll {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                }],
                Utc::now(),
            );

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

    #[tokio::test]
    async fn unknown_cancel_uses_session_queue_follow_up_not_persisted_request() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository.clone());
        let track_id = TrackId::new("btc-core");
        effect_worker_context
            .effect_worker_state
            .session_effect_queue
            .enqueue_transition_effects_for_test(
                &track_id,
                &[
                    TrackEffect::CancelOrder {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        order_id: "missing-order".into(),
                    },
                    submit_effect("downstream-submit"),
                ],
                Utc::now(),
            );

        let execution = Arc::new(UnknownCancelExecutionPort);
        let account = Arc::new(NoopAccountPort);
        let worker = EffectWorker::new(
            effect_worker_context,
            execution,
            account,
            Duration::from_millis(1),
        );

        worker.run_once().await.unwrap();

        assert!(
            worker
                .state
                .session_effect_queue
                .pending_effect_count_for_test(&track_id)
                == 0,
            "complete open-order sync should resolve the queue token and retire downstream session effects"
        );
    }

    #[tokio::test]
    async fn cancel_queue_blocked_action_is_reported_as_error() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        let execution = Arc::new(RecordingExecutionPort::default());
        let account = Arc::new(NoopAccountPort);
        let worker = EffectWorker::new(
            effect_worker_context,
            execution,
            account,
            Duration::from_millis(1),
        );

        let result = super::handle_cancel_queue_action(
            &worker,
            &TrackId::new("btc-core"),
            None,
            poise_application::CancelQueueAction::Blocked {
                reason: "queue invariant failed".into(),
            },
        )
        .await;

        assert!(
            result.is_err(),
            "blocked cancel queue actions must not be silently ignored"
        );
    }

    fn submit_effect(effect_id: &str) -> TrackEffect {
        TrackEffect::SubmitOrder {
            request: OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: Side::Buy,
                price: 100.0,
                quantity: 0.1,
                client_order_id: format!("{effect_id}-client"),
                reduce_only: false,
            },
            desired_exposure: Exposure(4.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            recovery_token: SubmitRecoveryToken::empty(),
        }
    }

    struct UnknownCancelExecutionPort;

    #[async_trait::async_trait]
    impl ExecutionPort for UnknownCancelExecutionPort {
        async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
            Ok(OrderReceipt {
                order_id: "test-order".into(),
                client_order_id: req.client_order_id,
                filled_qty: 0.0,
                status: poise_engine::ports::OrderStatus::New,
            })
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> Result<OrderReceipt> {
            Err(anyhow::Error::new(
                poise_engine::ports::ExecutionPortError::cancel_outcome_unknown(
                    "Unknown order sent.",
                ),
            ))
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Ok(())
        }

        async fn get_position(&self, instrument: &Instrument) -> Result<Position> {
            Ok(Position {
                instrument: instrument.clone(),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> Result<ExchangeOpenOrderSnapshot> {
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                Vec::new(),
            ))
        }
    }
}
