use std::sync::Arc;

use anyhow::{Error, Result};
use poise_engine::executor::SubmitRecoveryToken;
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
        request: OrderRequest,
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
        recovery_token: SubmitRecoveryToken,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitAttempt> {
        let recovery = self
            .executor
            .recover_submit_execution(id, effect_id, &recovery_token, live_order)
            .await?;

        Ok(submit_attempt_from_recovery(
            Arc::clone(&self.executor),
            id,
            effect_id,
            recovery,
        ))
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.executor.manager()
    }
}

fn submit_attempt_from_recovery(
    executor: Arc<MutationExecutor>,
    id: &str,
    effect_id: &str,
    recovery: SubmitExecutionRecovery,
) -> SubmitAttempt {
    match recovery {
        SubmitExecutionRecovery::Dispatch {
            request,
            desired_exposure,
        } => SubmitAttempt::Dispatch(SubmitDispatch::new(
            executor,
            TrackId::new(id),
            effect_id.to_string(),
            request,
            desired_exposure,
        )),
        SubmitExecutionRecovery::Finished(result) => SubmitAttempt::Finished(result),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::mutation_executor::test_support::{
        MemoryRepository, manager_with_pending_submit, seeded_manager, track_write_services,
    };
    use crate::{ApplicationNotification, EffectStatus};
    use poise_core::types::Exposure;
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::observation::MarketObservation;
    use poise_engine::ports::{
        ExchangeOrder, ExecutionQuote, OrderReceipt, OrderRequest, OrderStatus,
    };
    use poise_engine::track::{TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use tokio::time::timeout;

    use super::{
        SubmitDispatch, SubmitEffectService, SubmitExecutionRecovery, submit_attempt_from_recovery,
    };

    #[tokio::test]
    async fn submit_effect_service_recovers_or_dispatches_without_exposing_old_phase_methods() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone());

        timeout(
            Duration::from_secs(1),
            service.0.recover_or_dispatch(
                "btc-core",
                "btc-core:batch-1:0",
                SubmitRecoveryToken::empty(),
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
                filled_qty: 0.0,
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

    #[test]
    fn submit_attempt_from_recovery_uses_current_request_instead_of_stale_effect_request() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository);
        let current_request = OrderRequest {
            instrument: poise_engine::track::Instrument::new(Venue::Binance, "BTCUSDT"),
            side: poise_core::types::Side::Sell,
            price: 105.0,
            quantity: 0.2,
            client_order_id: "current-client".into(),
            reduce_only: true,
        };

        let attempt = submit_attempt_from_recovery(
            services.submit_effect.executor.clone(),
            "btc-core",
            "btc-core:batch-1:0",
            SubmitExecutionRecovery::Dispatch {
                request: current_request.clone(),
                desired_exposure: Exposure(-4.0),
            },
        );

        let super::SubmitAttempt::Dispatch(dispatch) = attempt else {
            panic!("expected dispatch");
        };
        assert_eq!(
            dispatch.request().client_order_id,
            current_request.client_order_id
        );
        assert_eq!(dispatch.request().price, current_request.price);
        assert_eq!(dispatch.request().quantity, current_request.quantity);
        assert_eq!(dispatch.request().reduce_only, current_request.reduce_only);
    }

    #[tokio::test]
    async fn submit_effect_service_recovers_live_order_into_current_binding_selected_by_recovery_token()
     {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .observation
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 104.9,
                        best_ask: 105.1,
                    },
                },
            )
            .await
            .unwrap();

        let submits = pending_submit_effects(&repository);
        assert!(submits.len() > 1, "test requires multiple submit effects");

        let stale = submits.first().unwrap().clone();
        let target = submits.last().unwrap().clone();
        assert_ne!(
            stale.request.client_order_id,
            target.request.client_order_id
        );

        let live_order = ExchangeOrder {
            instrument: stale.request.instrument.clone(),
            order_id: "live-order-1".into(),
            client_order_id: stale.request.client_order_id.clone(),
            side: stale.request.side,
            price: stale.request.price,
            qty: stale.request.quantity,
            filled_qty: 0.0,
            realized_pnl: 0.0,
            status: OrderStatus::New,
        };

        let attempt = services
            .submit_effect
            .recover_or_dispatch(
                "btc-core",
                &target.effect_id,
                target.recovery_token.clone(),
                Some(&live_order),
            )
            .await
            .unwrap();

        let super::SubmitAttempt::Finished(result) = attempt else {
            panic!("expected finished recovery");
        };
        assert!(result.invalidates_pending_submit());

        let manager = services.submit_effect.manager();
        let snapshot = manager
            .read()
            .await
            .mutation_frame("btc-core")
            .expect("track snapshot");
        let (order_id, status) = snapshot
            .binding_receipt_for_client_order_id(&target.request.client_order_id)
            .expect("target binding should still exist");
        assert_eq!(order_id.as_deref(), Some("live-order-1"));
        assert_eq!(status, poise_engine::executor::BindingStatus::Working);

        let persisted = repository
            .pending_effects()
            .into_iter()
            .find(|effect| effect.effect_id == target.effect_id)
            .expect("target effect should still be stored");
        assert_eq!(persisted.status, EffectStatus::Succeeded);
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

    #[derive(Clone)]
    struct SubmitFixture {
        effect_id: String,
        request: OrderRequest,
        recovery_token: SubmitRecoveryToken,
    }

    fn pending_submit_effects(repository: &MemoryRepository) -> Vec<SubmitFixture> {
        repository
            .pending_effects()
            .into_iter()
            .filter_map(|effect| match effect.effect {
                TrackEffect::SubmitOrder {
                    request,
                    recovery_token,
                    ..
                } => Some(SubmitFixture {
                    effect_id: effect.effect_id,
                    request,
                    recovery_token,
                }),
                _ => None,
            })
            .collect()
    }
}
