use std::sync::Arc;

use anyhow::{Error, Result};
#[cfg(any(test, feature = "server-test-support"))]
use poise_engine::manager::TrackManager;
use poise_engine::ports::{ExchangeOrder, OrderReceipt, OrderRequest};
use poise_engine::track::TrackId;
#[cfg(any(test, feature = "server-test-support"))]
use tokio::sync::RwLock;

use crate::mutation_executor::MutationExecutor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitAttemptResult {
    invalidates_pending_submit: bool,
}

impl SubmitAttemptResult {
    pub fn invalidates_pending_submit(&self) -> bool {
        self.invalidates_pending_submit
    }

    pub(crate) fn changed() -> Self {
        Self {
            invalidates_pending_submit: true,
        }
    }

    pub(crate) fn unchanged() -> Self {
        Self {
            invalidates_pending_submit: false,
        }
    }
}

pub enum SubmitAttempt {
    Dispatch(SubmitDispatch),
    Finished(SubmitAttemptResult),
}

/// 同一条 submit 生命周期只能结束一次；重复结束必须在类型层面不合法。
///
/// ```compile_fail
/// async fn finishes_twice(
///     dispatch: poise_application::submit_effect_service::SubmitDispatch,
/// ) {
///     let _ = dispatch.record_failure("first").await;
///     let _ = dispatch.record_completion_failure("second").await;
/// }
/// ```
pub struct SubmitDispatch {
    executor: Arc<MutationExecutor>,
    track_id: TrackId,
    effect_id: String,
    request: OrderRequest,
    desired_exposure: poise_core::types::Exposure,
}

impl SubmitDispatch {
    fn new(
        executor: Arc<MutationExecutor>,
        track_id: TrackId,
        effect_id: String,
        request: OrderRequest,
        desired_exposure: poise_core::types::Exposure,
    ) -> Self {
        Self {
            executor,
            track_id,
            effect_id,
            request,
            desired_exposure,
        }
    }

    pub fn request(&self) -> &OrderRequest {
        &self.request
    }

    pub async fn record_receipt(
        self,
        receipt: &OrderReceipt,
    ) -> std::result::Result<SubmitAttemptResult, SubmitReceiptWritebackFailure> {
        let SubmitDispatch {
            executor,
            track_id,
            effect_id,
            request,
            desired_exposure,
        } = self;
        match executor
            .complete_submit_execution(
                track_id.as_str(),
                &effect_id,
                &request,
                desired_exposure.clone(),
                receipt,
            )
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => Err(SubmitReceiptWritebackFailure {
                error,
                dispatch: SubmitDispatch {
                    executor,
                    track_id,
                    effect_id,
                    request,
                    desired_exposure,
                },
            }),
        }
    }

    pub async fn record_failure(self, error: &str) -> Result<SubmitAttemptResult> {
        self.executor
            .record_submit_failure(
                self.track_id.as_str(),
                &self.effect_id,
                &self.request.client_order_id,
                error,
            )
            .await
    }

    pub async fn record_completion_failure(self, error: &str) -> Result<SubmitAttemptResult> {
        self.executor
            .complete_submit_effect_failed(self.track_id.as_str(), &self.effect_id, error)
            .await
    }
}

pub struct SubmitReceiptWritebackFailure {
    error: Error,
    dispatch: SubmitDispatch,
}

impl SubmitReceiptWritebackFailure {
    pub fn into_parts(self) -> (Error, SubmitDispatch) {
        (self.error, self.dispatch)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SubmitExecutionRecovery {
    Dispatch {
        desired_exposure: poise_core::types::Exposure,
    },
    Finished(SubmitAttemptResult),
}

#[derive(Clone)]
pub struct SubmitEffectService {
    executor: Arc<MutationExecutor>,
}

impl SubmitEffectService {
    pub(crate) fn from_executor(executor: Arc<MutationExecutor>) -> Self {
        Self { executor }
    }

    pub async fn recover_or_dispatch(
        &self,
        id: &str,
        effect_id: &str,
        request: OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitAttempt> {
        Ok(
            match self
                .executor
                .recover_submit_execution(id, effect_id, &request, desired_exposure, live_order)
                .await?
            {
                SubmitExecutionRecovery::Dispatch { desired_exposure } => {
                    SubmitAttempt::Dispatch(SubmitDispatch::new(
                        Arc::clone(&self.executor),
                        TrackId::new(id),
                        effect_id.to_string(),
                        request,
                        desired_exposure,
                    ))
                }
                SubmitExecutionRecovery::Finished(result) => SubmitAttempt::Finished(result),
            },
        )
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.executor.manager()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::mutation_executor::test_support::{
        MemoryRepository, manager_with_pending_submit, track_write_services,
    };
    use crate::{ApplicationNotification, EffectStatus};
    use poise_core::types::Exposure;
    use poise_engine::ports::{OrderReceipt, OrderRequest, OrderStatus};
    use poise_engine::track::{TrackId, Venue};
    use tokio::time::timeout;

    use super::{SubmitDispatch, SubmitEffectService};

    #[tokio::test]
    async fn submit_effect_service_recovers_or_dispatches_without_exposing_old_phase_methods() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone());

        timeout(
            Duration::from_secs(1),
            service.0.recover_or_dispatch(
                "btc-core",
                "btc-core:batch-1:0",
                OrderRequest {
                    instrument: poise_engine::track::Instrument::new(Venue::Binance, "BTCUSDT"),
                    side: poise_core::types::Side::Buy,
                    price: 100.0,
                    quantity: 0.1,
                    client_order_id: "client-1".into(),
                    reduce_only: false,
                },
                Exposure(4.0),
                None,
            ),
        )
        .await
        .expect("submit attempt preparation should complete promptly")
        .unwrap();
    }

    #[tokio::test]
    async fn submit_dispatch_handle_records_failure_and_exposes_invalidation() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone());
        let dispatch = SubmitDispatch::new(
            service.0.executor.clone(),
            TrackId::new("btc-core"),
            "btc-core:batch-1:0".into(),
            OrderRequest {
                instrument: poise_engine::track::Instrument::new(Venue::Binance, "BTCUSDT"),
                side: poise_core::types::Side::Buy,
                price: 100.0,
                quantity: 0.1,
                client_order_id: "client-1".into(),
                reduce_only: false,
            },
            Exposure(4.0),
        );

        let outcome = timeout(
            Duration::from_secs(1),
            dispatch.record_failure("submit order rejected"),
        )
        .await
        .expect("submit failure writeback should complete promptly")
        .unwrap();

        assert!(outcome.invalidates_pending_submit());
        assert_eq!(repository.pending_effects()[0].status, EffectStatus::Failed);
    }

    #[tokio::test]
    async fn submit_dispatch_receipt_failure_returns_dispatch_for_follow_up_cleanup() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository);
        let dispatch = SubmitDispatch::new(
            service.0.executor.clone(),
            TrackId::new("missing-track"),
            "btc-core:batch-1:0".into(),
            OrderRequest {
                instrument: poise_engine::track::Instrument::new(Venue::Binance, "BTCUSDT"),
                side: poise_core::types::Side::Buy,
                price: 100.0,
                quantity: 0.1,
                client_order_id: "client-1".into(),
                reduce_only: false,
            },
            Exposure(4.0),
        );

        let failure = timeout(
            Duration::from_secs(1),
            dispatch.record_receipt(&OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            }),
        )
        .await
        .expect("submit receipt writeback should complete promptly")
        .expect_err("missing track should fail receipt writeback");
        let (error, dispatch) = failure.into_parts();

        assert!(error.to_string().contains("missing-track"));
        assert_eq!(dispatch.request().client_order_id, "client-1");
        assert!(
            dispatch
                .record_completion_failure("cleanup failed")
                .await
                .is_err(),
            "returned dispatch should remain usable for follow-up cleanup"
        );
    }

    fn test_service(
        repository: Arc<MemoryRepository>,
    ) -> (
        SubmitEffectService,
        tokio::sync::broadcast::Sender<ApplicationNotification>,
    ) {
        repository.seed_pending_submit_effect();
        let (services, notifications) =
            track_write_services(manager_with_pending_submit(), repository);
        (services.submit_effect, notifications)
    }
}
