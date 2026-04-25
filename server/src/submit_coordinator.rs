use std::sync::Arc;

use anyhow::{Error, Result};
use poise_application::SessionTrackEffect;
use poise_application::submit_effect_service::{
    SubmitAttempt, SubmitAttemptResult, SubmitDispatch, SubmitEffectService,
};
use poise_engine::executor::SubmitRecoveryToken;
use poise_engine::ports::{ExecutionPort, OrderReceipt, OrderRequest};

use crate::submit_preflight::{SubmitPreflight, SubmitPreflightDecision};

#[derive(Clone)]
pub struct SubmitCoordinator {
    execution: Arc<dyn ExecutionPort>,
    submit_effect_service: Arc<SubmitEffectService>,
    submit_preflight: Arc<SubmitPreflight>,
}

impl SubmitCoordinator {
    pub fn new(
        execution: Arc<dyn ExecutionPort>,
        submit_effect_service: Arc<SubmitEffectService>,
        submit_preflight: Arc<SubmitPreflight>,
    ) -> Self {
        Self {
            execution,
            submit_effect_service,
            submit_preflight,
        }
    }

    pub async fn prepare(
        &self,
        effect: &SessionTrackEffect,
        request: OrderRequest,
        recovery_token: SubmitRecoveryToken,
    ) -> Result<Option<SubmitFlight>> {
        let preflight_decision = self
            .submit_preflight
            .decide(&effect.effect_id, &request.client_order_id)
            .await;
        let live_order = match preflight_decision {
            SubmitPreflightDecision::Direct => None,
            SubmitPreflightDecision::NeedsLiveOrderLookup { .. } => Some(
                self.execution
                    .get_open_orders(&request.instrument)
                    .await?
                    .into_orders()
                    .into_iter()
                    .find(|order| order.client_order_id == request.client_order_id),
            )
            .flatten(),
        };

        let attempt = self
            .submit_effect_service
            .recover_or_dispatch(
                effect.track_id.as_str(),
                &effect.effect_id,
                recovery_token,
                live_order.as_ref(),
            )
            .await?;

        Ok(match attempt {
            SubmitAttempt::Dispatch(dispatch) => {
                self.submit_preflight
                    .mark_submit_started(&effect.effect_id)
                    .await;
                Some(SubmitFlight::new(
                    dispatch,
                    Arc::clone(&self.submit_preflight),
                ))
            }
            SubmitAttempt::Finished(result) => {
                apply_submit_attempt_result(&self.submit_preflight, result);
                None
            }
        })
    }
}

pub struct SubmitFlight {
    request: OrderRequest,
    completion: SubmitCompletion,
}

impl SubmitFlight {
    fn new(dispatch: SubmitDispatch, submit_preflight: Arc<SubmitPreflight>) -> Self {
        Self {
            request: dispatch.request().clone(),
            completion: SubmitCompletion::new(dispatch, submit_preflight),
        }
    }

    pub fn into_parts(self) -> (OrderRequest, SubmitCompletion) {
        (self.request, self.completion)
    }
}

pub struct SubmitCompletion {
    dispatch: SubmitDispatch,
    submit_preflight: Arc<SubmitPreflight>,
}

impl SubmitCompletion {
    fn new(dispatch: SubmitDispatch, submit_preflight: Arc<SubmitPreflight>) -> Self {
        Self {
            dispatch,
            submit_preflight,
        }
    }

    pub async fn record_receipt(
        self,
        receipt: &OrderReceipt,
    ) -> std::result::Result<(), SubmitReceiptWritebackFailure> {
        let SubmitCompletion {
            dispatch,
            submit_preflight,
        } = self;
        match dispatch.record_receipt(receipt).await {
            Ok(result) => {
                apply_submit_attempt_result(&submit_preflight, result);
                Ok(())
            }
            Err(writeback_failure) => {
                let (error, dispatch) = writeback_failure.into_parts();
                Err(SubmitReceiptWritebackFailure {
                    error,
                    completion: SubmitCompletion {
                        dispatch,
                        submit_preflight,
                    },
                })
            }
        }
    }

    pub async fn record_failure(self, error: &str) -> Result<()> {
        let result = self.dispatch.record_failure(error).await?;
        apply_submit_attempt_result(&self.submit_preflight, result);
        Ok(())
    }

    pub async fn record_completion_failure(self, error: &str) -> Result<()> {
        let result = self.dispatch.record_completion_failure(error).await?;
        apply_submit_attempt_result(&self.submit_preflight, result);
        Ok(())
    }
}

pub struct SubmitReceiptWritebackFailure {
    error: Error,
    completion: SubmitCompletion,
}

impl SubmitReceiptWritebackFailure {
    pub fn into_parts(self) -> (Error, SubmitCompletion) {
        (self.error, self.completion)
    }
}

fn apply_submit_attempt_result(submit_preflight: &SubmitPreflight, result: SubmitAttemptResult) {
    if result.invalidates_pending_submit() {
        submit_preflight.mark_pending_submit_effects_dirty();
    }
}
