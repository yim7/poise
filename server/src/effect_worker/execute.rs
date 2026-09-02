use anyhow::{Result, anyhow};
use chrono::Utc;
use poise_application::{
    CancelReceiptResolution, SessionEffectOutcome, SessionTrackEffect, TrackInput,
    TrackUpdateReceipt, is_loaded_track_invariant_violation,
};
use poise_engine::executor::SubmitRecoveryToken;
use poise_engine::ports::{OrderReceipt, OrderRequest};

use super::dispatch::SessionDispatchResult;
use super::{Cancellation, EffectWorker, is_insufficient_margin_failure};
use crate::exchange_freshness::FreshnessGateDecision;
use crate::order_outcome::{
    OutcomeClass, OutcomeUnknownRecovery, cancel_writeback_outcome_unknown, classify_cancel_error,
    classify_submit_error, classify_submit_receipt_writeback_error,
};
use crate::submit_coordinator::SubmitCoordinator;

pub(super) async fn execute_submit(
    worker: &EffectWorker,
    effect: &SessionTrackEffect,
    request: OrderRequest,
    recovery_token: SubmitRecoveryToken,
    _desired_exposure: poise_core::types::Exposure,
) -> Result<SessionEffectOutcome> {
    let submit = SubmitCoordinator::new(
        worker.execution.clone(),
        worker.state.submit_effect_service.clone(),
        worker.state.reconcile.submit_preflight.clone(),
    );

    if matches!(
        worker
            .state
            .reconcile
            .exchange_freshness
            .decide_effect(effect.track_id.as_str(), &effect.effect)
            .await,
        FreshnessGateDecision::ReconcileFirst
    ) {
        worker
            .trigger_immediate_reconcile(
                effect.track_id.as_str(),
                &request.instrument,
                crate::order_outcome::ReconcileReason::SyncBeforeSideEffect,
            )
            .await?;
        return Ok(SessionEffectOutcome::Deferred {
            until: poise_application::DeferredUntil::ExchangeState,
        });
    }

    let Some(flight) = submit.prepare(effect, request, recovery_token).await? else {
        return Ok(SessionEffectOutcome::Finished);
    };
    let (request, completion) = flight.into_parts();

    match worker.execution.submit_order(request.clone()).await {
        Ok(receipt) => {
            if !worker
                .state
                .session_effect_queue
                .record_submit_awaiting_resolution(&effect.effect_id)
            {
                return Ok(SessionEffectOutcome::Blocked {
                    reason: format!(
                        "submit effect `{}` was not in an in-flight queue state",
                        effect.effect_id
                    ),
                });
            }
            if let Err(writeback_failure) = completion.record_receipt(&receipt).await {
                let (error, completion) = writeback_failure.into_parts();
                if let OutcomeClass::OutcomeUnknown(recovery) =
                    classify_submit_receipt_writeback_error(&error)
                {
                    return Ok(defer_submit_until_exchange_state(
                        worker,
                        effect.track_id.as_str(),
                        &request.instrument,
                        recovery,
                    )
                    .await);
                }
                completion
                    .record_completion_failure(&error.to_string())
                    .await?;
                return Err(error);
            }
            Ok(SessionEffectOutcome::Finished)
        }
        Err(error) => {
            if let OutcomeClass::OutcomeUnknown(recovery) = classify_submit_error(&error) {
                return Ok(defer_unknown_submit_dispatch_until_exchange_state(
                    worker, effect, &request, recovery,
                )
                .await);
            }
            if is_insufficient_margin_failure(&error) {
                worker
                    .state
                    .account_margin_guard
                    .activate_insufficient_margin(
                        &request.instrument,
                        "insufficient_margin",
                        Utc::now(),
                    );
                match worker
                    .account
                    .get_account_capacity_snapshot(&request.instrument)
                    .await
                {
                    Ok(snapshot) => {
                        worker
                            .state
                            .account_margin_guard
                            .update_snapshot(request.instrument.clone(), snapshot);
                    }
                    Err(error) => {
                        tracing::warn!(
                            instrument = %request.instrument.symbol,
                            venue = %request.instrument.venue.as_str(),
                            "failed to refresh account capacity snapshot after insufficient margin: {error}"
                        );
                    }
                }
            }
            let failure_message = error.to_string();
            match completion.record_failure(&failure_message).await {
                Ok(()) => Ok(()),
                Err(clear_error) if is_loaded_track_invariant_violation(&clear_error) => {
                    Err(clear_error)
                }
                Err(clear_error) => Err(anyhow!(
                    "submit order failed: {error}; failed to record submit failure: {clear_error}"
                )),
            }?;
            Ok(SessionEffectOutcome::Blocked {
                reason: failure_message,
            })
        }
    }
}

async fn defer_unknown_submit_dispatch_until_exchange_state(
    worker: &EffectWorker,
    effect: &SessionTrackEffect,
    request: &OrderRequest,
    recovery: OutcomeUnknownRecovery,
) -> SessionEffectOutcome {
    let recovery_error = worker
        .recover_unknown_outcome(effect.track_id.as_str(), &request.instrument, recovery)
        .await
        .err();
    let initial_snapshot_confirmed = recovery_error.is_none();

    match worker
        .state
        .submit_effect_service
        .record_outcome_unknown(
            effect.track_id.as_str(),
            &effect.effect_id,
            &request.client_order_id,
            initial_snapshot_confirmed,
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::error!(
                track_id = %effect.track_id.as_str(),
                effect_id = %effect.effect_id,
                "submit outcome became unknown outside the expected in-flight queue state"
            );
        }
        Err(error) => {
            tracing::warn!(
                track_id = %effect.track_id.as_str(),
                effect_id = %effect.effect_id,
                "failed to record unknown submit outcome; retaining deferred execution: {error}"
            );
        }
    }

    if worker
        .state
        .session_effect_queue
        .active_submit_effect_ids()
        .contains(&effect.effect_id)
    {
        worker
            .state
            .reconcile
            .exchange_freshness
            .mark_stale(effect.track_id.as_str(), recovery.freshness_reason)
            .await;
    }

    if let Some(error) = recovery_error {
        tracing::warn!(
            track_id = %effect.track_id.as_str(),
            symbol = %request.instrument.symbol,
            venue = %request.instrument.venue.as_str(),
            "failed to reconcile unknown submit outcome; retaining submit ownership: {error}"
        );
    }

    SessionEffectOutcome::Deferred {
        until: poise_application::DeferredUntil::ExchangeState,
    }
}

async fn defer_submit_until_exchange_state(
    worker: &EffectWorker,
    track_id: &str,
    instrument: &poise_core::track::Instrument,
    recovery: OutcomeUnknownRecovery,
) -> SessionEffectOutcome {
    if let Err(error) = worker
        .recover_unknown_outcome(track_id, instrument, recovery)
        .await
    {
        tracing::warn!(
            track_id,
            symbol = %instrument.symbol,
            venue = %instrument.venue.as_str(),
            "failed to reconcile unknown submit outcome; retaining submit ownership: {error}"
        );
    }
    SessionEffectOutcome::Deferred {
        until: poise_application::DeferredUntil::ExchangeState,
    }
}

pub(super) async fn execute_cancellation(
    worker: &EffectWorker,
    effect: &SessionTrackEffect,
    cancellation: Cancellation,
) -> Result<SessionDispatchResult> {
    let instrument = cancellation.instrument().clone();
    if matches!(
        worker
            .state
            .reconcile
            .exchange_freshness
            .decide_effect(effect.track_id.as_str(), &effect.effect)
            .await,
        FreshnessGateDecision::ReconcileFirst
    ) {
        worker
            .trigger_immediate_reconcile(
                effect.track_id.as_str(),
                &instrument,
                crate::order_outcome::ReconcileReason::SyncBeforeSideEffect,
            )
            .await?;
        return Ok(SessionDispatchResult::Outcome(
            SessionEffectOutcome::Deferred {
                until: poise_application::DeferredUntil::ExchangeState,
            },
        ));
    }
    let result = match cancellation {
        Cancellation::One {
            ref instrument,
            ref order_id,
        } => worker
            .execution
            .cancel_order(instrument, order_id)
            .await
            .map(CancellationResult::One),
        Cancellation::All { ref instrument } => worker
            .execution
            .cancel_all(instrument)
            .await
            .map(|_| CancellationResult::All),
    };

    match result {
        Ok(result) => {
            let writeback: Result<SessionDispatchResult> = match &cancellation {
                Cancellation::One { order_id, .. } => {
                    let CancellationResult::One(receipt) = &result else {
                        unreachable!("single cancel should produce a single cancel receipt");
                    };
                    let resolution = worker
                        .state
                        .reconcile
                        .runtime
                        .handle(
                            &effect.track_id,
                            TrackInput::CancelOrderSucceeded {
                                effect_id: effect.effect_id.clone(),
                                order_id: order_id.clone(),
                                receipt: receipt.clone(),
                            },
                        )
                        .await?;
                    let TrackUpdateReceipt::CancelOrderResolved(resolution) = resolution else {
                        unreachable!("cancel receipt input must return its update receipt");
                    };
                    Ok(SessionDispatchResult::Cancel(resolution))
                }
                Cancellation::All { .. } => worker
                    .state
                    .reconcile
                    .runtime
                    .handle(
                        &effect.track_id,
                        TrackInput::CancelAllSucceeded {
                            effect_id: effect.effect_id.clone(),
                        },
                    )
                    .await
                    .map(|_| SessionDispatchResult::Outcome(SessionEffectOutcome::Finished)),
            };
            let result = match writeback {
                Ok(result) => result,
                Err(error) => {
                    worker
                        .recover_unknown_outcome(
                            effect.track_id.as_str(),
                            &instrument,
                            cancel_writeback_outcome_unknown(),
                        )
                        .await?;
                    return Err(error);
                }
            };
            Ok(result)
        }
        Err(error) => {
            if let OutcomeClass::OutcomeUnknown(_recovery) = classify_cancel_error(&error)
                && let Cancellation::One { order_id, .. } = &cancellation
            {
                return Ok(SessionDispatchResult::Cancel(
                    CancelReceiptResolution::Unknown {
                        order_id: order_id.clone(),
                        reason: error.to_string(),
                    },
                ));
            }
            worker
                .state
                .reconcile
                .runtime
                .handle(
                    &effect.track_id,
                    TrackInput::EffectFailed {
                        effect_id: effect.effect_id.clone(),
                        error: error.to_string(),
                    },
                )
                .await?;
            Err(error.into())
        }
    }
}

enum CancellationResult {
    One(OrderReceipt),
    All,
}
