use std::sync::Arc;

use crate::exchange::{ExecutionPort, OrderReceipt, OrderRequest};
use anyhow::{Error, Result};
use poise_core::track::TrackId;
use poise_engine::executor::SubmitRecoveryToken;
use poise_engine::manager::SubmitRecoveryOrderLookup;
#[cfg(any(test, feature = "server-test-support"))]
use poise_engine::manager::TrackManager;
#[cfg(any(test, feature = "server-test-support"))]
use tokio::sync::RwLock;

use crate::trading_runtime::{
    SubmitRecoveryReceipt, TrackInput, TrackUpdateReceipt, TradingRuntime,
};
use crate::{SessionTrackEffect, session_effect_queue::SubmitDispatchPreparation};

pub(crate) enum SubmitAttempt {
    Dispatch(SubmitDispatch),
    Finished,
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
    runtime: Arc<TradingRuntime>,
    track_id: TrackId,
    effect_id: String,
    request: OrderRequest,
    desired_exposure: poise_core::types::Exposure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitReceiptOutcome {
    Recorded,
    QueueOwnershipLost,
}

impl SubmitDispatch {
    fn new(
        runtime: Arc<TradingRuntime>,
        track_id: TrackId,
        effect_id: String,
        request: OrderRequest,
        desired_exposure: poise_core::types::Exposure,
    ) -> Self {
        Self {
            runtime,
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
    ) -> std::result::Result<SubmitReceiptOutcome, SubmitReceiptWritebackFailure> {
        let SubmitDispatch {
            runtime,
            track_id,
            effect_id,
            request,
            desired_exposure,
        } = self;
        if !runtime
            .session_effect_queue()
            .record_submit_awaiting_resolution(&effect_id)
        {
            return Ok(SubmitReceiptOutcome::QueueOwnershipLost);
        }
        match runtime
            .handle(
                &track_id,
                TrackInput::SubmitCompleted {
                    effect_id: effect_id.clone(),
                    request: request.clone(),
                    desired_exposure: desired_exposure.clone(),
                    receipt: receipt.clone(),
                },
            )
            .await
        {
            Ok(TrackUpdateReceipt::SubmitAttemptFinished) => Ok(SubmitReceiptOutcome::Recorded),
            Ok(_) => unreachable!("submit completion must return a submit attempt receipt"),
            Err(error) => Err(SubmitReceiptWritebackFailure {
                error,
                dispatch: SubmitDispatch {
                    runtime,
                    track_id,
                    effect_id,
                    request,
                    desired_exposure,
                },
            }),
        }
    }

    pub async fn record_failure(self, error: &str) -> Result<()> {
        match self
            .runtime
            .handle(
                &self.track_id,
                TrackInput::SubmitFailed {
                    effect_id: self.effect_id,
                    client_order_id: self.request.client_order_id,
                    error: error.to_string(),
                },
            )
            .await?
        {
            TrackUpdateReceipt::SubmitAttemptFinished => Ok(()),
            _ => unreachable!("submit failure must return a submit attempt receipt"),
        }
    }

    pub async fn record_completion_failure(self, error: &str) -> Result<()> {
        match self
            .runtime
            .handle(
                &self.track_id,
                TrackInput::SubmitEffectFailed {
                    effect_id: self.effect_id,
                    error: error.to_string(),
                },
            )
            .await?
        {
            TrackUpdateReceipt::SubmitAttemptFinished => Ok(()),
            _ => unreachable!("submit effect failure must return a submit attempt receipt"),
        }
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

#[derive(Clone)]
pub struct SubmitEffectService {
    runtime: Arc<TradingRuntime>,
}

impl SubmitEffectService {
    pub fn new(runtime: Arc<TradingRuntime>) -> Self {
        Self { runtime }
    }

    pub async fn prepare(
        &self,
        execution: &dyn ExecutionPort,
        effect: &SessionTrackEffect,
        request: OrderRequest,
        recovery_token: SubmitRecoveryToken,
    ) -> Result<Option<SubmitDispatch>> {
        let preparation = self
            .runtime
            .session_effect_queue()
            .prepare_submit_dispatch(&effect.effect_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "submit effect `{}` is not the claimed queue effect",
                    effect.effect_id
                )
            })?;
        let order_lookup = match preparation {
            SubmitDispatchPreparation::Direct => SubmitRecoveryOrderLookup::NotQueried,
            SubmitDispatchPreparation::NeedsLiveOrderLookup { client_order_id } => {
                SubmitRecoveryOrderLookup::Complete(
                    execution
                        .get_open_orders(&request.instrument)
                        .await?
                        .into_orders()
                        .into_iter()
                        .find(|order| order.client_order_id == client_order_id),
                )
            }
        };

        Ok(
            match self
                .recover_or_dispatch(
                    effect.track_id.as_str(),
                    &effect.effect_id,
                    recovery_token,
                    order_lookup,
                )
                .await?
            {
                SubmitAttempt::Dispatch(dispatch) => Some(dispatch),
                SubmitAttempt::Finished => None,
            },
        )
    }

    pub(crate) async fn recover_or_dispatch(
        &self,
        id: &str,
        effect_id: &str,
        recovery_token: SubmitRecoveryToken,
        order_lookup: SubmitRecoveryOrderLookup,
    ) -> Result<SubmitAttempt> {
        let recovery = self
            .runtime
            .handle(
                &TrackId::new(id),
                TrackInput::RecoverSubmit {
                    effect_id: effect_id.to_string(),
                    recovery_token,
                    order_lookup,
                },
            )
            .await?;
        let TrackUpdateReceipt::SubmitRecovery(recovery) = recovery else {
            unreachable!("submit recovery must return a recovery receipt");
        };

        Ok(submit_attempt_from_recovery(
            Arc::clone(&self.runtime),
            id,
            effect_id,
            recovery,
        ))
    }

    pub async fn record_outcome_unknown(
        &self,
        id: &str,
        effect_id: &str,
        client_order_id: &str,
        initial_snapshot_confirmed: bool,
    ) -> Result<bool> {
        match self
            .runtime
            .handle(
                &TrackId::new(id),
                TrackInput::SubmitOutcomeUnknown {
                    effect_id: effect_id.to_string(),
                    client_order_id: client_order_id.to_string(),
                    initial_snapshot_confirmed,
                },
            )
            .await?
        {
            TrackUpdateReceipt::SubmitOutcomeUnknownRecorded { changed } => Ok(changed),
            _ => unreachable!("unknown submit outcome must return its update receipt"),
        }
    }

    pub fn is_submit_active(&self, effect_id: &str) -> bool {
        self.runtime
            .session_effect_queue()
            .is_active_submit_effect(effect_id)
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.runtime.manager()
    }
}

fn submit_attempt_from_recovery(
    runtime: Arc<TradingRuntime>,
    id: &str,
    effect_id: &str,
    recovery: SubmitRecoveryReceipt,
) -> SubmitAttempt {
    match recovery {
        SubmitRecoveryReceipt::Dispatch {
            request,
            desired_exposure,
        } => SubmitAttempt::Dispatch(SubmitDispatch::new(
            runtime,
            TrackId::new(id),
            effect_id.to_string(),
            request,
            desired_exposure,
        )),
        SubmitRecoveryReceipt::Finished => SubmitAttempt::Finished,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::exchange::{ExchangeOrder, ExecutionQuote, OrderReceipt, OrderRequest, OrderStatus};
    use crate::trading_runtime::test_support::{
        MemoryRepository, manager_with_pending_submit, seeded_manager, track_write_services,
    };
    use crate::{ApplicationNotification, EffectStatus};
    use poise_core::instrument::PositionQty;
    use poise_core::track::{TrackId, Venue};
    use poise_core::types::Exposure;
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::observation::MarketObservation;
    use tokio::time::timeout;

    use super::{
        SubmitDispatch, SubmitEffectService, SubmitRecoveryOrderLookup, SubmitRecoveryReceipt,
        submit_attempt_from_recovery,
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
                SubmitRecoveryOrderLookup::NotQueried,
            ),
        )
        .await
        .expect("submit attempt preparation should complete promptly")
        .unwrap();
    }

    #[tokio::test]
    async fn submit_dispatch_handle_records_failure() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone());
        let dispatch = SubmitDispatch::new(
            service.0.runtime.clone(),
            TrackId::new("btc-core"),
            "btc-core:batch-1:0".into(),
            OrderRequest {
                instrument: poise_core::track::Instrument::new(Venue::Binance, "BTCUSDT"),
                side: poise_core::types::Side::Buy,
                price: 100.0,
                quantity: PositionQty::new(0.1).unwrap(),
                client_order_id: "client-1".into(),
                reduce_only: false,
            },
            Exposure(4.0),
        );

        timeout(
            Duration::from_secs(1),
            dispatch.record_failure("submit order rejected"),
        )
        .await
        .expect("submit failure writeback should complete promptly")
        .unwrap();
        assert_eq!(repository.pending_effects()[0].status, EffectStatus::Failed);
    }

    #[tokio::test]
    async fn submit_dispatch_receipt_failure_returns_dispatch_for_follow_up_cleanup() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository);
        let request = OrderRequest {
            instrument: poise_core::track::Instrument::new(Venue::Binance, "BTCUSDT"),
            side: poise_core::types::Side::Buy,
            price: 100.0,
            quantity: PositionQty::new(0.1).unwrap(),
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let queue = service.0.runtime.session_effect_queue();
        let effect_id = queue
            .enqueue_transition_effects(
                &TrackId::new("missing-track"),
                &[TrackEffect::SubmitOrder {
                    request: request.clone(),
                    desired_exposure: Exposure(4.0),
                    submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                    recovery_token: SubmitRecoveryToken::empty(),
                }],
                chrono::Utc::now(),
            )
            .effect_ids()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(queue.claim_next().unwrap().effect_id, effect_id);
        let dispatch = SubmitDispatch::new(
            service.0.runtime.clone(),
            TrackId::new("missing-track"),
            effect_id,
            request,
            Exposure(4.0),
        );

        let failure = timeout(
            Duration::from_secs(1),
            dispatch.record_receipt(&OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                filled_qty: PositionQty::default(),
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
            instrument: poise_core::track::Instrument::new(Venue::Binance, "BTCUSDT"),
            side: poise_core::types::Side::Sell,
            price: 105.0,
            quantity: PositionQty::new(0.2).unwrap(),
            client_order_id: "current-client".into(),
            reduce_only: true,
        };

        let attempt = submit_attempt_from_recovery(
            services.submit_effect.runtime.clone(),
            "btc-core",
            "btc-core:batch-1:0",
            SubmitRecoveryReceipt::Dispatch {
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
    async fn submit_effect_service_recovers_live_order_with_recovery_token_request() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .runtime
            .handle_market_for_test(
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
        assert!(!submits.is_empty(), "test requires a submit effect");

        let target = submits.first().unwrap().clone();
        assert_ne!(
            target.recovery_token,
            SubmitRecoveryToken::empty(),
            "submit recovery should use stable binding identity"
        );

        let live_order = ExchangeOrder {
            instrument: target.request.instrument.clone(),
            order_id: "live-order-1".into(),
            client_order_id: "stale-live-client".into(),
            side: target.request.side,
            price: target.request.price,
            qty: target.request.quantity,
            filled_qty: PositionQty::default(),
            status: OrderStatus::New,
        };

        let attempt = services
            .submit_effect
            .recover_or_dispatch(
                "btc-core",
                &target.effect_id,
                target.recovery_token.clone(),
                SubmitRecoveryOrderLookup::Complete(Some(live_order)),
            )
            .await
            .unwrap();

        let super::SubmitAttempt::Finished = attempt else {
            panic!("expected finished recovery");
        };

        let manager = services.submit_effect.manager();
        let snapshot = manager
            .read()
            .await
            .get_track("btc-core")
            .cloned()
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

    #[tokio::test]
    async fn authorized_submit_stays_matchable_when_protection_precedes_writeback() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .runtime
            .handle_market_for_test(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
            )
            .await
            .unwrap();

        let target = pending_submit_effects(&repository)
            .into_iter()
            .next()
            .expect("test requires a submit effect");
        let claimed = services
            .session_effect_queue
            .claim_next()
            .expect("submit effect should be queued");
        assert_eq!(claimed.effect_id, target.effect_id);

        let attempt = services
            .submit_effect
            .recover_or_dispatch(
                "btc-core",
                &target.effect_id,
                target.recovery_token.clone(),
                SubmitRecoveryOrderLookup::NotQueried,
            )
            .await
            .unwrap();
        let super::SubmitAttempt::Dispatch(dispatch) = attempt else {
            panic!("submit should be authorized for dispatch");
        };

        services
            .runtime
            .handle_market_for_test(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 84.9,
                        best_ask: 85.1,
                    },
                },
            )
            .await
            .unwrap();

        assert!(
            services
                .session_effect_queue
                .record_submit_awaiting_resolution(&target.effect_id),
            "accepted submit should still own the in-flight queue item"
        );

        let writeback = dispatch
            .record_receipt(&OrderReceipt {
                order_id: "accepted-before-protection".into(),
                client_order_id: target.request.client_order_id.clone(),
                filled_qty: PositionQty::default(),
                status: OrderStatus::New,
            })
            .await;
        if let Err(failure) = writeback {
            let (error, _dispatch) = failure.into_parts();
            panic!(
                "an authorized submit must retain its binding through receipt writeback: {error}"
            );
        }

        let manager = services.submit_effect.manager();
        let snapshot = manager
            .read()
            .await
            .get_track("btc-core")
            .cloned()
            .expect("track snapshot");
        let (order_id, status) = snapshot
            .binding_receipt_for_client_order_id(&target.request.client_order_id)
            .expect("accepted submit should remain bound until cancellation completes");
        assert_eq!(order_id.as_deref(), Some("accepted-before-protection"));
        assert_eq!(status, poise_engine::executor::BindingStatus::CancelPending);
        assert!(repository.pending_effects().iter().any(|effect| matches!(
            &effect.effect,
            TrackEffect::CancelOrder { order_id, .. }
                if order_id == "accepted-before-protection"
        )));
    }

    #[tokio::test]
    async fn protection_before_submit_authorization_supersedes_effect_without_dispatch() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .runtime
            .handle_market_for_test(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
            )
            .await
            .unwrap();
        let target = pending_submit_effects(&repository)
            .into_iter()
            .next()
            .expect("test requires a submit effect");

        services
            .runtime
            .handle_market_for_test(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 84.9,
                        best_ask: 85.1,
                    },
                },
            )
            .await
            .unwrap();

        let attempt = services
            .submit_effect
            .recover_or_dispatch(
                "btc-core",
                &target.effect_id,
                target.recovery_token,
                SubmitRecoveryOrderLookup::NotQueried,
            )
            .await
            .unwrap();
        let super::SubmitAttempt::Finished = attempt else {
            panic!("withdrawn submit must not receive dispatch capability");
        };
        let persisted = repository
            .pending_effects()
            .into_iter()
            .find(|effect| effect.effect_id == target.effect_id)
            .expect("submit journal entry should remain diagnosable");
        assert_eq!(persisted.status, EffectStatus::Superseded);
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
