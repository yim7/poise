use anyhow::{Result, anyhow};
use chrono::Utc;
use poise_application::{
    FollowUpRetirementRequest, PersistedTrackEffect, is_loaded_track_invariant_violation,
};
use poise_engine::executor::SubmitRecoveryToken;
use poise_engine::ports::OrderRequest;

use super::{Cancellation, EffectWorker, is_insufficient_margin_failure};
use crate::exchange_freshness::FreshnessGateDecision;
use crate::order_outcome::{
    OutcomeClass, cancel_writeback_outcome_unknown, classify_cancel_error,
    classify_submit_receipt_writeback_error,
};
use crate::submit_coordinator::SubmitCoordinator;

pub(super) async fn execute_submit(
    worker: &EffectWorker,
    persisted: &PersistedTrackEffect,
    request: OrderRequest,
    recovery_token: SubmitRecoveryToken,
    _desired_exposure: poise_core::types::Exposure,
) -> Result<()> {
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
            .decide_effect(persisted.track_id.as_str(), &persisted.effect)
            .await,
        FreshnessGateDecision::ReconcileFirst
    ) {
        worker
            .trigger_immediate_reconcile(
                persisted.track_id.as_str(),
                &request.instrument,
                crate::order_outcome::ReconcileReason::SyncBeforeSideEffect,
            )
            .await?;
        return Ok(());
    }

    let Some(flight) = submit.prepare(persisted, request, recovery_token).await? else {
        return Ok(());
    };
    let (request, completion) = flight.into_parts();

    match worker.execution.submit_order(request.clone()).await {
        Ok(receipt) => {
            if let Err(writeback_failure) = completion.record_receipt(&receipt).await {
                let (error, completion) = writeback_failure.into_parts();
                if let OutcomeClass::OutcomeUnknown(recovery) =
                    classify_submit_receipt_writeback_error(&error)
                {
                    worker
                        .recover_unknown_outcome(
                            persisted.track_id.as_str(),
                            &request.instrument,
                            recovery,
                        )
                        .await?;
                    return Err(error);
                }
                completion
                    .record_completion_failure(&error.to_string())
                    .await?;
                return Err(error);
            }
            Ok(())
        }
        Err(error) => {
            let failure_message = error.to_string();
            if is_insufficient_margin_failure(&failure_message) {
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
            match completion.record_failure(&failure_message).await {
                Ok(()) => Err(anyhow!(failure_message)),
                Err(clear_error) if is_loaded_track_invariant_violation(&clear_error) => {
                    Err(clear_error)
                }
                Err(clear_error) => Err(anyhow!(
                    "submit order failed: {error}; failed to record submit failure: {clear_error}"
                )),
            }
        }
    }
}

pub(super) async fn execute_cancellation(
    worker: &EffectWorker,
    persisted: &PersistedTrackEffect,
    cancellation: Cancellation,
) -> Result<()> {
    let instrument = cancellation.instrument().clone();
    if matches!(
        worker
            .state
            .reconcile
            .exchange_freshness
            .decide_effect(persisted.track_id.as_str(), &persisted.effect)
            .await,
        FreshnessGateDecision::ReconcileFirst
    ) {
        worker
            .trigger_immediate_reconcile(
                persisted.track_id.as_str(),
                &instrument,
                crate::order_outcome::ReconcileReason::SyncBeforeSideEffect,
            )
            .await?;
        return Ok(());
    }
    let result = match cancellation {
        Cancellation::One {
            ref instrument,
            ref order_id,
        } => worker.execution.cancel_order(instrument, order_id).await,
        Cancellation::All { ref instrument } => worker.execution.cancel_all(instrument).await,
    };

    match result {
        Ok(()) => {
            let writeback: Result<()> = match &cancellation {
                Cancellation::One { order_id, .. } => {
                    worker
                        .state
                        .effect_service
                        .record_cancel_order_success(
                            persisted.track_id.as_str(),
                            &persisted.effect_id,
                            &persisted.batch_id,
                            persisted.sequence,
                            order_id,
                        )
                        .await
                }
                Cancellation::All { .. } => {
                    worker
                        .state
                        .effect_service
                        .record_cancel_all_success(
                            persisted.track_id.as_str(),
                            &persisted.effect_id,
                        )
                        .await
                }
            };
            if let Err(error) = writeback {
                worker
                    .recover_unknown_outcome(
                        persisted.track_id.as_str(),
                        &instrument,
                        cancel_writeback_outcome_unknown(),
                    )
                    .await?;
                return Err(error);
            }
            Ok(())
        }
        Err(error) => {
            if let OutcomeClass::OutcomeUnknown(recovery) = classify_cancel_error(&error) {
                worker
                    .recover_unknown_outcome(persisted.track_id.as_str(), &instrument, recovery)
                    .await?;
                if let Cancellation::One { order_id, .. } = &cancellation
                    && let Err(retirement_error) = worker
                        .state
                        .effect_service
                        .request_follow_up_retirement(
                            persisted.track_id.as_str(),
                            FollowUpRetirementRequest {
                                batch_id: persisted.batch_id.clone(),
                                blocked_sequence: persisted.sequence,
                                closed_order_id: order_id.clone(),
                            },
                        )
                        .await
                {
                    tracing::warn!(
                        track_id = %persisted.track_id.as_str(),
                        order_id = %order_id,
                        "failed to request follow-up retirement after unknown cancel outcome: {retirement_error}"
                    );
                }
            }
            worker
                .state
                .effect_service
                .complete_effect_failed(
                    persisted.track_id.as_str(),
                    &persisted.effect_id,
                    &error.to_string(),
                )
                .await?;
            Err(error)
        }
    }
}
