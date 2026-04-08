use anyhow::{Result, anyhow};
use chrono::Utc;
use poise_application::{
    FollowUpRetirementRequest, PersistedTrackEffect, PreparedSubmitExecution,
    is_loaded_track_invariant_violation,
};
use poise_engine::ports::OrderRequest;

use crate::exchange_freshness::FreshnessGateDecision;
use crate::order_outcome::{
    OutcomeClass, classify_cancel_error, classify_submit_receipt_writeback_error,
};
use crate::submit_preflight::SubmitPreflightDecision;

use super::{Cancellation, EffectWorker, is_insufficient_margin_failure};

pub(super) async fn execute_submit(
    worker: &EffectWorker,
    persisted: &PersistedTrackEffect,
    request: OrderRequest,
    desired_exposure: poise_core::types::Exposure,
) -> Result<()> {
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

    let preflight_decision = worker
        .state
        .reconcile
        .submit_preflight
        .decide(&persisted.effect_id, &request.client_order_id)
        .await;
    let Some(prepared_submit) = worker
        .prepare_submit_execution(
            persisted,
            &request,
            desired_exposure.clone(),
            preflight_decision,
        )
        .await?
    else {
        return Ok(());
    };
    worker
        .state
        .reconcile
        .submit_preflight
        .mark_submit_started(&persisted.effect_id)
        .await;

    match worker.execution.submit_order(request.clone()).await {
        Ok(receipt) => {
            if let Err(error) = worker
                .state
                .effect_service
                .complete_submit_execution(
                    persisted.track_id.as_str(),
                    &persisted.effect_id,
                    &request,
                    prepared_submit.desired_exposure,
                    &receipt,
                )
                .await
            {
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
                return Err(error);
            }

            Ok(())
        }
        Err(error) => {
            let failure_message = error.to_string();
            if is_insufficient_margin_failure(&failure_message) {
                worker.state.account_margin_guard.activate_insufficient_margin(
                    &request.instrument,
                    "insufficient_margin",
                    Utc::now(),
                );
                if let Ok(snapshot) = worker
                    .account
                    .get_account_capacity_snapshot(&request.instrument)
                    .await
                {
                    worker
                        .state
                        .account_margin_guard
                        .update_snapshot(request.instrument.clone(), snapshot);
                }
            }
            match worker
                .state
                .effect_service
                .record_submit_failure(
                    persisted.track_id.as_str(),
                    &persisted.effect_id,
                    &request.client_order_id,
                    &failure_message,
                )
                .await
            {
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

pub(super) async fn prepare_submit_execution(
    worker: &EffectWorker,
    persisted: &PersistedTrackEffect,
    request: &OrderRequest,
    desired_exposure: poise_core::types::Exposure,
    preflight_decision: SubmitPreflightDecision,
) -> Result<Option<PreparedSubmitExecution>> {
    let live_order = match preflight_decision {
        SubmitPreflightDecision::Direct => None,
        SubmitPreflightDecision::NeedsLiveOrderLookup { .. } => Some(
            worker
                .execution
                .get_open_orders(&request.instrument)
                .await?
                .into_iter()
                .find(|order| order.client_order_id == request.client_order_id),
        )
        .flatten(),
    };

    worker
        .state
        .effect_service
        .prepare_submit_execution(
            persisted.track_id.as_str(),
            &persisted.effect_id,
            request,
            desired_exposure.clone(),
            live_order.as_ref(),
        )
        .await
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
                    .state
                    .effect_service
                    .complete_effect_failed(
                        persisted.track_id.as_str(),
                        &persisted.effect_id,
                        &error.to_string(),
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
