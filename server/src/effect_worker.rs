use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::Utc;
use poise_engine::ports::{
    ExchangePort, FollowUpRetirementRequest, OrderRequest, PersistedTrackEffect,
};
use poise_engine::track::Instrument;
use poise_engine::transition::TrackEffect;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::assembly::ServerState;
use crate::order_outcome::{
    OutcomeClass, ReconcileRequest, classify_cancel_error, classify_submit_receipt_writeback_error,
};
use crate::runtime;
use crate::submit_preflight::SubmitPreflightDecision;
#[derive(Clone)]
pub struct EffectWorker {
    state: ServerState,
    exchange: Arc<dyn ExchangePort>,
    poll_interval: Duration,
    shutdown_rx: watch::Receiver<bool>,
}

impl EffectWorker {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        poll_interval: Duration,
    ) -> Self {
        let (_, shutdown_rx) = watch::channel(false);
        Self::with_shutdown_rx(state, exchange, poll_interval, shutdown_rx)
    }

    pub fn with_shutdown_rx(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        poll_interval: Duration,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            state,
            exchange,
            poll_interval,
            shutdown_rx,
        }
    }

    pub fn spawn(&self) -> JoinHandle<()> {
        let worker = self.clone();
        tokio::spawn(async move {
            if let Err(error) = worker.run_until_shutdown().await {
                tracing::warn!("effect worker iteration failed: {error}");
            }
        })
    }

    pub async fn run_until_shutdown(&self) -> Result<()> {
        let mut shutdown_rx = self.shutdown_rx.clone();

        loop {
            if *shutdown_rx.borrow() {
                return Ok(());
            }

            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        return Ok(());
                    }
                }
                _ = sleep(self.poll_interval) => {
                    self.run_once().await?;
                }
            }
        }
    }

    pub async fn run_once(&self) -> Result<()> {
        let mut seen_effects = HashSet::new();

        loop {
            if *self.shutdown_rx.borrow() {
                break;
            }

            let Some(effect) = self
                .state
                .state_repository
                .list_dispatchable_effects()
                .await?
                .into_iter()
                .find(|effect| !seen_effects.contains(&effect.effect_id))
            else {
                break;
            };
            let effect_id = effect.effect_id.clone();
            if let Err(error) = self.process_effect(effect).await {
                tracing::warn!("failed to process persisted effect: {error}");
            }
            seen_effects.insert(effect_id);
        }

        Ok(())
    }

    async fn process_effect(&self, persisted: PersistedTrackEffect) -> Result<()> {
        match persisted.effect {
            TrackEffect::SubmitOrder {
                ref request,
                ref desired_exposure,
            } => {
                self.execute_submit(&persisted, request.clone(), desired_exposure.clone())
                    .await
            }
            TrackEffect::CancelOrder {
                ref instrument,
                ref order_id,
            } => {
                self.execute_cancellation(
                    &persisted,
                    Cancellation::One {
                        instrument: instrument.clone(),
                        order_id: order_id.clone(),
                    },
                )
                .await
            }
            TrackEffect::CancelAll { ref instrument } => {
                self.execute_cancellation(
                    &persisted,
                    Cancellation::All {
                        instrument: instrument.clone(),
                    },
                )
                .await
            }
            TrackEffect::NoOp => {
                self.state
                    .write_service
                    .complete_effect_succeeded(persisted.track_id.as_str(), &persisted.effect_id)
                    .await?;
                Ok(())
            }
        }
    }

    async fn execute_submit(
        &self,
        persisted: &PersistedTrackEffect,
        request: OrderRequest,
        desired_exposure: poise_core::types::Exposure,
    ) -> Result<()> {
        let preflight_decision = self
            .state
            .submit_preflight
            .decide(&persisted.effect_id, &request.client_order_id)
            .await;
        let Some(prepared_submit) = self
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
        self.state
            .submit_preflight
            .mark_submit_started(&persisted.effect_id)
            .await;

        match self.exchange.submit_order(request.clone()).await {
            Ok(receipt) => {
                if let Err(error) = self
                    .state
                    .write_service
                    .complete_submit_execution(
                        persisted.track_id.as_str(),
                        &persisted.effect_id,
                        &request,
                        prepared_submit.desired_exposure,
                        &receipt,
                    )
                    .await
                {
                    if let OutcomeClass::OutcomeUnknown(reason) =
                        classify_submit_receipt_writeback_error(&error)
                    {
                        runtime::enqueue_reconcile_request(
                            &self.state,
                            &self.exchange,
                            ReconcileRequest {
                                track_id: persisted.track_id.as_str().to_string(),
                                reason,
                            },
                            &request.instrument,
                        )
                        .await?;
                    }
                    self.state
                        .write_service
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
                    self.state
                        .account_margin_guard
                        .activate_insufficient_margin(
                            &request.instrument,
                            "insufficient_margin",
                            Utc::now(),
                        );
                    if let Ok(snapshot) = self
                        .exchange
                        .get_account_margin_snapshot(&request.instrument)
                        .await
                    {
                        self.state
                            .account_margin_guard
                            .update_snapshot(request.instrument.clone(), snapshot);
                    }
                }
                match self
                    .state
                    .write_service
                    .record_submit_failure(
                        persisted.track_id.as_str(),
                        &persisted.effect_id,
                        &request.client_order_id,
                        &failure_message,
                    )
                    .await
                {
                    Ok(()) => Err(anyhow!(failure_message)),
                    Err(clear_error)
                        if crate::write_service::is_loaded_track_invariant_violation(
                            &clear_error,
                        ) =>
                    {
                        Err(clear_error)
                    }
                    Err(clear_error) => Err(anyhow!(
                        "submit order failed: {error}; failed to record submit failure: {clear_error}"
                    )),
                }
            }
        }
    }

    async fn prepare_submit_execution(
        &self,
        persisted: &PersistedTrackEffect,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        preflight_decision: SubmitPreflightDecision,
    ) -> Result<Option<crate::write_service::PreparedSubmitExecution>> {
        let live_order = match preflight_decision {
            SubmitPreflightDecision::Direct => None,
            SubmitPreflightDecision::NeedsLiveOrderLookup { .. } => Some(
                self.exchange
                    .get_open_orders(&request.instrument)
                    .await?
                    .into_iter()
                    .find(|order| order.client_order_id == request.client_order_id),
            )
            .flatten(),
        };

        self.state
            .write_service
            .prepare_submit_execution(
                persisted.track_id.as_str(),
                &persisted.effect_id,
                request,
                desired_exposure.clone(),
                live_order.as_ref(),
            )
            .await
    }

    async fn execute_cancellation(
        &self,
        persisted: &PersistedTrackEffect,
        cancellation: Cancellation,
    ) -> Result<()> {
        let instrument = cancellation.instrument().clone();
        let result = match cancellation {
            Cancellation::One {
                ref instrument,
                ref order_id,
            } => self.exchange.cancel_order(instrument, order_id).await,
            Cancellation::All { ref instrument } => self.exchange.cancel_all(instrument).await,
        };

        match result {
            Ok(()) => {
                let writeback = match &cancellation {
                    Cancellation::One { order_id, .. } => {
                        self.state
                            .write_service
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
                        self.state
                            .write_service
                            .record_cancel_all_success(
                                persisted.track_id.as_str(),
                                &persisted.effect_id,
                            )
                            .await
                    }
                };
                if let Err(error) = writeback {
                    self.state
                        .write_service
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
                if let OutcomeClass::OutcomeUnknown(reason) = classify_cancel_error(&error) {
                    runtime::enqueue_reconcile_request(
                        &self.state,
                        &self.exchange,
                        ReconcileRequest {
                            track_id: persisted.track_id.as_str().to_string(),
                            reason,
                        },
                        &instrument,
                    )
                    .await?;
                    if let Cancellation::One { order_id, .. } = &cancellation
                        && let Err(retirement_error) = self
                            .state
                            .write_service
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
                self.state
                    .write_service
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
}

fn is_insufficient_margin_failure(message: &str) -> bool {
    message.contains(r#""code":-2019"#) || message.contains("Margin is insufficient")
}

enum Cancellation {
    One {
        instrument: poise_engine::track::Instrument,
        order_id: String,
    },
    All {
        instrument: poise_engine::track::Instrument,
    },
}

impl Cancellation {
    fn instrument(&self) -> &Instrument {
        match self {
            Self::One { instrument, .. } | Self::All { instrument } => instrument,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};
    use poise_engine::executor::{ExecutionMode, ExecutionReason, RecoveryAnomaly};
    use poise_engine::manager::TrackManager;
    use poise_engine::observation::OrderObservation;
    use poise_engine::ports::{
        ClockPort, CommittedTrackWrite, EffectStatus, EffectStatusUpdate, ExchangeInfo,
        ExchangeOrder, ExchangePort, FollowUpRetirementRequest, OrderReceipt, OrderRequest,
        OrderStatus, PersistedTrackEffect, Position, StateRepositoryPort, StoredTrackEvent,
        StoredTrackSnapshot, TrackReadRepositoryPort,
    };
    use poise_engine::runtime::{
        ExecutionStats, ExecutorState, RiskState, SlotState, TrackStatus, WorkingOrder,
    };
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, watch};
    use tokio::time::timeout;

    use crate::assembly::build_server_state;
    use crate::projector::TrackProjector;
    use crate::query_service::TrackQueryService;
    use crate::submit_preflight::{SubmitPreflight, SubmitPreflightDecision};
    use crate::write_service::TrackWriteService;

    use super::{Cancellation, EffectWorker};

    #[tokio::test]
    async fn submit_success_updates_working_order_via_receipt_writeback() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::default());
        let state = test_state(repository.clone(), exchange.clone()).await;

        let transition = state
            .write_service
            .observe_market("btc-core", 95.0)
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [TrackEffect::SubmitOrder { .. }]
        ));

        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.snapshot("btc-core").unwrap();
            snapshot
                .executor_state
                .slots
                .push(poise_engine::runtime::ExecutionSlot {
                    slot: poise_engine::executor::OrderSlot::new("inventory_followup"),
                    state: SlotState::Empty,
                    working_order: None,
                });
            manager.restore_track_state(&snapshot).unwrap();
            repository.seed_snapshot("btc-core", snapshot).await;
        }

        let worker = EffectWorker::new(
            state.clone(),
            exchange as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        worker.run_once().await.unwrap();

        let manager_handle = state.write_service.manager();
        let manager = manager_handle.read().await;
        let snapshot = manager.snapshot("btc-core").unwrap();
        let slot = snapshot
            .executor_state
            .slots
            .first()
            .expect("submit receipt should update working order slot");
        assert_eq!(slot.state, SlotState::Working);
        let order = slot
            .working_order
            .as_ref()
            .expect("slot should keep working order after receipt");
        assert_eq!(order.order_id.as_deref(), Some("order-1"));
        assert_eq!(order.status, OrderStatus::New);
        assert_eq!(snapshot.executor_state.slots.len(), 2);
        assert_eq!(
            snapshot.executor_state.slots[1].slot,
            poise_engine::executor::OrderSlot::new("inventory_followup")
        );
        assert_eq!(snapshot.executor_state.slots[1].state, SlotState::Empty);

        let effect = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("submit effect should remain persisted");
        assert_eq!(effect.status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn effect_worker_writeback_keeps_round_target_without_working_order_target_copy() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::default());
        let state = test_state(repository.clone(), exchange.clone()).await;

        let transition = state
            .write_service
            .observe_market("btc-core", 95.0)
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [TrackEffect::SubmitOrder { .. }]
        ));
        let expected_round_target = match transition.effects.as_slice() {
            [
                TrackEffect::SubmitOrder {
                    desired_exposure, ..
                },
            ] => desired_exposure.clone(),
            _ => panic!("expected a single submit effect"),
        };

        let worker = EffectWorker::new(
            state.clone(),
            exchange as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        worker.run_once().await.unwrap();

        let manager_handle = state.write_service.manager();
        let manager = manager_handle.read().await;
        let snapshot = manager.snapshot("btc-core").unwrap();
        let executor = serde_json::to_value(&snapshot).unwrap()["executor_state"]
            .as_object()
            .expect("executor state should serialize as an object")
            .clone();
        let active_round = executor
            .get("active_round")
            .and_then(|value| value.as_object())
            .expect("receipt writeback should preserve active_round");
        let working_order = executor["slots"][0]["working_order"]
            .as_object()
            .expect("working order should be present after receipt");

        assert_eq!(
            active_round["desired_exposure"],
            serde_json::json!(expected_round_target.0)
        );
        assert!(
            !working_order.contains_key("desired_exposure"),
            "working order should not keep a target copy after writeback"
        );
    }

    #[tokio::test]
    async fn fresh_submit_uses_direct_preflight_without_open_orders_lookup() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::default());
        let state = test_state(repository, exchange.clone()).await;

        let transition = state
            .write_service
            .observe_market("btc-core", 95.0)
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [TrackEffect::SubmitOrder { .. }]
        ));

        let worker = EffectWorker::new(
            state,
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        worker.run_once().await.unwrap();

        assert_eq!(exchange.get_open_orders_calls(), 0);
    }

    #[tokio::test]
    async fn mark_submit_started_happens_only_after_prepare_returns_some() {
        let repository = Arc::new(MemoryRepository::default());
        let submit_started = Arc::new(Notify::new());
        let release_submit = Arc::new(Notify::new());
        let exchange = Arc::new(FakeExchange::with_blocked_submit(
            submit_started.clone(),
            release_submit.clone(),
        ));
        let state = test_state(repository.clone(), exchange.clone()).await;

        let transition = state
            .write_service
            .observe_market("btc-core", 95.0)
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [TrackEffect::SubmitOrder { .. }]
        ));
        let effect_id = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("submit effect should be persisted")
            .effect_id;

        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        let task = tokio::spawn(async move { worker.run_once().await });
        submit_started.notified().await;
        let attempted_after_prepare = state.submit_preflight.is_attempted(&effect_id).await;
        release_submit.notify_waiters();
        task.await.unwrap().unwrap();

        assert!(attempted_after_prepare);

        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::default());
        let state = test_state(repository.clone(), exchange.clone()).await;

        repository
            .seed_snapshot("btc-core", snapshot_with_recovery_anomaly())
            .await;
        let skipped_effect_id = "btc-core:skip:0".to_string();
        repository
            .seed_effect(PersistedTrackEffect {
                effect_id: skipped_effect_id.clone(),
                track_id: TrackId::new("btc-core"),
                batch_id: "skip".into(),
                sequence: 0,
                effect: TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "BTCUSDT-skip".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;
        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            manager
                .restore_track_state(&snapshot_with_recovery_anomaly())
                .unwrap();
        }

        let worker = EffectWorker::new(
            state.clone(),
            exchange as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        worker.run_once().await.unwrap();

        assert!(
            !state
                .submit_preflight
                .is_attempted(&skipped_effect_id)
                .await
        );
    }

    #[tokio::test]
    async fn submit_preflight_assumes_single_effect_worker_execution_order() {
        let preflight = SubmitPreflight::new();
        preflight.mark_submit_started("effect-1").await;

        let started_decision = preflight.decide("effect-1", "client-1").await;
        let fresh_decision = preflight.decide("effect-2", "client-2").await;

        assert_eq!(
            started_decision,
            SubmitPreflightDecision::NeedsLiveOrderLookup {
                client_order_id: "client-1".into()
            }
        );
        assert_eq!(fresh_decision, SubmitPreflightDecision::Direct);
    }

    #[tokio::test]
    async fn submit_recovery_waits_while_recovery_anomaly_is_active() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::default());
        let state = test_state(repository.clone(), exchange.clone()).await;

        repository
            .seed_snapshot("btc-core", snapshot_with_recovery_anomaly())
            .await;
        repository
            .seed_effect(PersistedTrackEffect {
                effect_id: "btc-core:batch:0".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "batch".into(),
                sequence: 0,
                effect: TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            let snapshot = snapshot_with_recovery_anomaly();
            manager.restore_track_state(&snapshot).unwrap();
        }

        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        worker.run_once().await.unwrap();

        assert!(exchange.effects.lock().await.is_empty());
        let effect = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("submit effect should remain pending");
        assert_eq!(effect.status, EffectStatus::Pending);
    }

    #[tokio::test]
    async fn cancel_success_clears_working_order_slot_without_waiting_for_order_event() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::default());
        let state = test_state(repository.clone(), exchange.clone()).await;
        let snapshot = snapshot_with_working_order();

        repository.seed_snapshot("btc-core", snapshot.clone()).await;
        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository
            .seed_effect(PersistedTrackEffect {
                effect_id: "btc-core:batch:0".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "batch".into(),
                sequence: 0,
                effect: TrackEffect::CancelOrder {
                    instrument: btc_instrument(),
                    order_id: "order-1".into(),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let worker = EffectWorker::new(
            state.clone(),
            exchange as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        worker.run_once().await.unwrap();

        let manager_handle = state.write_service.manager();
        let manager = manager_handle.read().await;
        let snapshot = manager.snapshot("btc-core").unwrap();
        assert_eq!(
            snapshot.executor_state.slots,
            vec![poise_engine::runtime::ExecutionSlot {
                slot: poise_engine::executor::OrderSlot::new("inventory_core"),
                state: SlotState::Empty,
                working_order: None,
            }]
        );

        let effect = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("cancel effect should remain persisted");
        assert_eq!(effect.status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn cancel_unknown_order_sent_resyncs_exchange_state_before_marking_effect_failed() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::with_cancel_order_error(
            "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}",
        ));
        exchange.set_position_qty(15.0).await;
        let state = test_state(repository.clone(), exchange.clone()).await;
        let snapshot = snapshot_with_working_order();

        repository.seed_snapshot("btc-core", snapshot.clone()).await;
        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository
            .seed_effect(PersistedTrackEffect {
                effect_id: "btc-core:batch:0".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "batch".into(),
                sequence: 0,
                effect: TrackEffect::CancelOrder {
                    instrument: btc_instrument(),
                    order_id: "order-1".into(),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        worker.run_once().await.unwrap();

        let effect = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("cancel effect should remain persisted");
        assert_eq!(effect.status, EffectStatus::Failed);
        assert_eq!(exchange.get_position_calls(), 1);
        assert_eq!(exchange.get_open_orders_calls(), 1);
    }

    #[tokio::test]
    async fn cancel_unknown_order_sent_retires_follow_up_after_terminal_update_arrives() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::with_cancel_order_error(
            "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}",
        ));
        exchange.set_position_qty(15.0).await;
        exchange.open_orders.lock().await.push(ExchangeOrder {
            instrument: btc_instrument(),
            order_id: "order-1".into(),
            client_order_id: "client-1".into(),
            side: Side::Buy,
            price: 95.0,
            qty: 15.0,
            realized_pnl: 0.0,
            status: OrderStatus::New,
        });
        let state = test_state(repository.clone(), exchange.clone()).await;
        let snapshot = snapshot_with_working_order();

        repository.seed_snapshot("btc-core", snapshot.clone()).await;
        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        let cancel_effect = PersistedTrackEffect {
            effect_id: "btc-core:batch:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "batch".into(),
            sequence: 0,
            effect: TrackEffect::CancelOrder {
                instrument: btc_instrument(),
                order_id: "order-1".into(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        repository.seed_effect(cancel_effect.clone()).await;
        repository
            .seed_effect(PersistedTrackEffect {
                effect_id: "btc-core:batch:1".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "batch".into(),
                sequence: 1,
                effect: TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        client_order_id: "btc-core-replacement".into(),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: 0.4,
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(4.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        let error = worker
            .execute_cancellation(
                &cancel_effect,
                Cancellation::One {
                    instrument: btc_instrument(),
                    order_id: "order-1".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Unknown order sent."));
        assert_eq!(
            repository
                .list_all_effects()
                .await
                .iter()
                .find(|effect| effect.effect_id == "btc-core:batch:1")
                .map(|effect| effect.status),
            Some(EffectStatus::Pending)
        );

        state
            .write_service
            .observe_order_with_absorb_result(
                "btc-core",
                OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::Filled,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            repository
                .list_all_effects()
                .await
                .iter()
                .find(|effect| effect.effect_id == "btc-core:batch:1")
                .map(|effect| effect.status),
            Some(EffectStatus::Superseded)
        );
    }

    #[tokio::test]
    async fn cancel_unknown_order_sent_still_marks_cancel_effect_failed_when_follow_up_retry_errors()
     {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::with_cancel_order_error(
            "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}",
        ));
        exchange.set_position_qty(15.0).await;
        exchange.open_orders.lock().await.push(ExchangeOrder {
            instrument: btc_instrument(),
            order_id: "order-1".into(),
            client_order_id: "client-1".into(),
            side: Side::Buy,
            price: 95.0,
            qty: 15.0,
            realized_pnl: 0.0,
            status: OrderStatus::New,
        });
        let state = test_state(repository.clone(), exchange.clone()).await;
        let snapshot = snapshot_with_working_order();

        repository.seed_snapshot("btc-core", snapshot.clone()).await;
        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        let cancel_effect = PersistedTrackEffect {
            effect_id: "btc-core:broken:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "broken".into(),
            sequence: 0,
            effect: TrackEffect::CancelOrder {
                instrument: btc_instrument(),
                order_id: "order-1".into(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        repository.seed_effect(cancel_effect.clone()).await;
        repository
            .seed_effect(PersistedTrackEffect {
                effect_id: "btc-core:broken:1".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "broken".into(),
                sequence: 1,
                effect: TrackEffect::CancelOrder {
                    instrument: btc_instrument(),
                    order_id: "unexpected-cancel".into(),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        let error = worker
            .execute_cancellation(
                &cancel_effect,
                Cancellation::One {
                    instrument: btc_instrument(),
                    order_id: "order-1".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Unknown order sent."));
        assert_eq!(
            repository
                .list_all_effects()
                .await
                .iter()
                .find(|effect| effect.effect_id == "btc-core:broken:0")
                .map(|effect| effect.status),
            Some(EffectStatus::Failed)
        );
        assert!(
            repository
                .list_all_effects()
                .await
                .iter()
                .find(|effect| effect.effect_id == "btc-core:broken:0")
                .and_then(|effect| effect.last_error.as_deref())
                .is_some_and(|error| error.contains("Unknown order sent."))
        );
    }

    #[tokio::test]
    async fn submit_recovery_proceed_keeps_active_pending_target_when_rounded_request_matches() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::default());
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        };
        let exchange_rules = ExchangeRules {
            price_tick: 10.0,
            quantity_step: 1.0,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        let state = test_state_with_track(
            repository.clone(),
            exchange.clone(),
            config.clone(),
            exchange_rules,
        )
        .await;
        let snapshot = snapshot_with_submit_pending_order(
            94.99,
            config.clone(),
            WorkingOrder {
                order_id: None,
                client_order_id: "btc-core-reconcile".into(),
                side: Side::Buy,
                price: 90.0,
                quantity: 4.0,
                status: OrderStatus::Submitting,
                role: poise_engine::executor::OrderRole::IncreaseInventory,
            },
        );
        let expected_round_target = snapshot
            .desired_exposure
            .clone()
            .expect("snapshot should carry desired exposure");

        repository.seed_snapshot("btc-core", snapshot.clone()).await;
        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository
            .seed_effect(PersistedTrackEffect {
                effect_id: "btc-core:batch:0".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "batch".into(),
                sequence: 0,
                effect: TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 90.0,
                        quantity: 4.0,
                        client_order_id: "btc-core-reconcile".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(4.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let worker = EffectWorker::new(
            state.clone(),
            exchange as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        worker.run_once().await.unwrap();

        let manager_handle = state.write_service.manager();
        let manager = manager_handle.read().await;
        let snapshot = manager.snapshot("btc-core").unwrap();
        assert_eq!(
            snapshot
                .executor_state
                .active_round
                .as_ref()
                .map(|round| round.desired_exposure.clone()),
            Some(expected_round_target)
        );
    }

    #[tokio::test]
    async fn effect_worker_stops_polling_new_effects_after_shutdown_signal() {
        let repository = Arc::new(MemoryRepository::default());
        let submit_started = Arc::new(Notify::new());
        let release_submit = Arc::new(Notify::new());
        let exchange = Arc::new(FakeExchange::with_blocked_submit(
            submit_started.clone(),
            release_submit.clone(),
        ));
        let state = test_state(repository.clone(), exchange.clone()).await;

        let transition = state
            .write_service
            .observe_market("btc-core", 95.0)
            .await
            .unwrap();
        let submit_effect = match transition.effects.as_slice() {
            [TrackEffect::SubmitOrder { .. }] => repository
                .list_all_effects()
                .await
                .into_iter()
                .next()
                .expect("submit effect should be persisted"),
            other => panic!("expected one submit effect, got {other:?}"),
        };
        repository
            .seed_effect(PersistedTrackEffect {
                effect_id: "btc-core:shutdown:0".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "shutdown".into(),
                sequence: 0,
                effect: TrackEffect::NoOp,
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = EffectWorker::with_shutdown_rx(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(1),
            shutdown_rx,
        );
        let task = worker.spawn();

        submit_started.notified().await;
        shutdown_tx.send(true).unwrap();
        release_submit.notify_waiters();

        timeout(Duration::from_secs(1), async {
            task.await.unwrap();
        })
        .await
        .unwrap();

        let effects = repository.list_all_effects().await;
        let submit = effects
            .iter()
            .find(|effect| effect.effect_id == submit_effect.effect_id)
            .expect("submit effect should still exist");
        let no_op = effects
            .iter()
            .find(|effect| effect.effect_id == "btc-core:shutdown:0")
            .expect("no-op effect should still exist");

        assert_eq!(exchange.effects.lock().await.len(), 1);
        assert_eq!(submit.status, EffectStatus::Succeeded);
        assert_eq!(no_op.status, EffectStatus::Pending);
    }

    #[tokio::test]
    async fn submit_receipt_unmatched_resyncs_exchange_state_before_marking_effect_failed() {
        let repository = Arc::new(MemoryRepository::default());
        let submit_started = Arc::new(Notify::new());
        let release_submit = Arc::new(Notify::new());
        let exchange = Arc::new(FakeExchange::with_blocked_submit(
            submit_started.clone(),
            release_submit.clone(),
        ));
        exchange.set_position_qty(15.0).await;
        let state = test_state(repository.clone(), exchange.clone()).await;

        let transition = state
            .write_service
            .observe_market("btc-core", 95.0)
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [TrackEffect::SubmitOrder { .. }]
        ));

        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_secs(60),
        );
        let task = tokio::spawn(async move { worker.run_once().await });

        submit_started.notified().await;

        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.snapshot("btc-core").unwrap();
            snapshot.executor_state.slots = vec![poise_engine::runtime::ExecutionSlot {
                slot: poise_engine::executor::OrderSlot::new("inventory_core"),
                state: SlotState::Empty,
                working_order: None,
            }];
            manager.restore_track_state(&snapshot).unwrap();
        }

        release_submit.notify_waiters();
        task.await.unwrap().unwrap();

        let manager_handle = state.write_service.manager();
        let manager = manager_handle.read().await;
        let snapshot = manager.snapshot("btc-core").unwrap();
        assert_eq!(snapshot.current_exposure, Exposure(4.0));
        assert_eq!(snapshot.desired_exposure, Some(Exposure(4.0)));
        assert!(
            snapshot
                .executor_state
                .diagnostics
                .recovery_anomaly
                .is_none()
        );

        let effect = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("submit effect should remain persisted");
        assert_eq!(effect.status, EffectStatus::Failed);
        assert!(
            effect
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("submit receipt did not match executor slot"))
        );
        assert_eq!(exchange.get_position_calls(), 1);
        assert_eq!(exchange.get_open_orders_calls(), 1);
    }

    async fn test_state(
        repository: Arc<MemoryRepository>,
        exchange: Arc<FakeExchange>,
    ) -> crate::assembly::ServerState {
        test_state_with_track(
            repository,
            exchange,
            test_config(),
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.1,
                min_qty: 0.0,
                min_notional: 0.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
        )
        .await
    }

    async fn test_state_with_track(
        repository: Arc<MemoryRepository>,
        _exchange: Arc<FakeExchange>,
        config: TrackConfig,
        exchange_rules: ExchangeRules,
    ) -> crate::assembly::ServerState {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = TrackManager::new(clock);
        let instrument = btc_instrument();
        manager
            .add_track(
                TrackId::new("btc-core"),
                instrument.clone(),
                config,
                test_budget(),
                exchange_rules,
            )
            .unwrap();

        let (notifications, _) = broadcast::channel(16);
        let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
        let read_repository: Arc<dyn TrackReadRepositoryPort> = repository;
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            state_repository.clone(),
            notifications.clone(),
        ));
        build_server_state(
            write_service,
            state_repository,
            Arc::new(TrackQueryService::new(read_repository)),
            Arc::new(TrackProjector::new()),
        )
    }

    fn btc_instrument() -> Instrument {
        Instrument::new(Venue::Binance, "BTCUSDT")
    }

    fn snapshot_with_recovery_anomaly() -> TrackRuntimeSnapshot {
        TrackRuntimeSnapshot {
            track_id: TrackId::new("btc-core"),
            instrument: btc_instrument(),
            config: test_config(),
            status: TrackStatus::Active,
            current_exposure: Exposure(0.0),
            desired_exposure: Some(Exposure(6.0)),
            manual_target_override: None,
            executor_state: ExecutorState {
                active_round: Some(poise_engine::runtime::ExecutionRound {
                    desired_exposure: Exposure(6.0),
                    mode: ExecutionMode::Passive,
                    started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                }),
                diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                    mode: ExecutionMode::Passive,
                    inventory_gap: Exposure(6.0),
                    gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                    last_reprice_at: None,
                    last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                    recovery_anomaly: Some(RecoveryAnomaly::UnknownLiveOrder),
                },
                slots: vec![poise_engine::runtime::ExecutionSlot {
                    slot: poise_engine::executor::OrderSlot::new("inventory_core"),
                    state: SlotState::Empty,
                    working_order: None,
                }],
                recent_terminal_orders: Vec::new(),
                stats: ExecutionStats {
                    started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                    max_inventory_gap_abs: Exposure(6.0),
                    max_gap_age_ms: 0,
                },
            },
            replacement_gate_reason: None,
            risk: RiskState::default(),
            observed: ObservedState {
                reference_price: Some(95.0),
                out_of_band_since: None,
                last_tick_at: None,
                market_data_stale_since: None,
            },
        }
    }

    fn snapshot_with_working_order() -> TrackRuntimeSnapshot {
        TrackRuntimeSnapshot {
            track_id: TrackId::new("btc-core"),
            instrument: btc_instrument(),
            config: test_config(),
            status: TrackStatus::Active,
            current_exposure: Exposure(2.0),
            desired_exposure: Some(Exposure(6.0)),
            manual_target_override: None,
            executor_state: ExecutorState {
                active_round: Some(poise_engine::runtime::ExecutionRound {
                    desired_exposure: Exposure(6.0),
                    mode: ExecutionMode::Passive,
                    started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                }),
                diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                    mode: ExecutionMode::Passive,
                    inventory_gap: Exposure(4.0),
                    gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                    last_reprice_at: None,
                    last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                    recovery_anomaly: None,
                },
                slots: vec![poise_engine::runtime::ExecutionSlot {
                    slot: poise_engine::executor::OrderSlot::new("inventory_core"),
                    state: SlotState::Working,
                    working_order: Some(poise_engine::runtime::WorkingOrder {
                        order_id: Some("order-1".into()),
                        client_order_id: "client-1".into(),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: 15.0,
                        status: OrderStatus::New,
                        role: poise_engine::executor::OrderRole::IncreaseInventory,
                    }),
                }],
                recent_terminal_orders: Vec::new(),
                stats: ExecutionStats {
                    started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                    max_inventory_gap_abs: Exposure(4.0),
                    max_gap_age_ms: 0,
                },
            },
            replacement_gate_reason: None,
            risk: RiskState::default(),
            observed: ObservedState {
                reference_price: Some(95.0),
                out_of_band_since: None,
                last_tick_at: None,
                market_data_stale_since: None,
            },
        }
    }

    fn snapshot_with_submit_pending_order(
        reference_price: f64,
        config: TrackConfig,
        order: WorkingOrder,
    ) -> TrackRuntimeSnapshot {
        TrackRuntimeSnapshot {
            track_id: TrackId::new("btc-core"),
            instrument: btc_instrument(),
            config: config.clone(),
            status: TrackStatus::Active,
            current_exposure: Exposure(0.0),
            desired_exposure: Some(poise_core::strategy::desired_exposure(
                reference_price,
                &config,
            )),
            manual_target_override: None,
            executor_state: ExecutorState {
                active_round: Some(poise_engine::runtime::ExecutionRound {
                    desired_exposure: poise_core::strategy::desired_exposure(
                        reference_price,
                        &config,
                    ),
                    mode: ExecutionMode::Passive,
                    started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                }),
                diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                    mode: ExecutionMode::Passive,
                    inventory_gap: Exposure(
                        poise_core::strategy::desired_exposure(reference_price, &config).0,
                    ),
                    gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                    last_reprice_at: None,
                    last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                    recovery_anomaly: None,
                },
                slots: vec![poise_engine::runtime::ExecutionSlot {
                    slot: poise_engine::executor::OrderSlot::new("inventory_core"),
                    state: SlotState::SubmitPending,
                    working_order: Some(order),
                }],
                recent_terminal_orders: Vec::new(),
                stats: ExecutionStats {
                    started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                    max_inventory_gap_abs: Exposure(0.0),
                    max_gap_age_ms: 0,
                },
            },
            replacement_gate_reason: None,
            risk: RiskState::default(),
            observed: ObservedState {
                reference_price: Some(reference_price),
                out_of_band_since: None,
                last_tick_at: None,
                market_data_stale_since: None,
            },
        }
    }

    fn test_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn test_budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 10.0,
        }
    }

    struct FixedClock(chrono::DateTime<Utc>);

    impl ClockPort for FixedClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            self.0
        }
    }

    struct FakeExchange {
        effects: AsyncMutex<Vec<OrderRequest>>,
        submit_started: Option<Arc<Notify>>,
        release_submit: Option<Arc<Notify>>,
        cancel_order_error: Option<String>,
        position: AsyncMutex<Position>,
        open_orders: AsyncMutex<Vec<ExchangeOrder>>,
        get_position_calls: AtomicUsize,
        get_open_orders_calls: AtomicUsize,
    }

    impl FakeExchange {
        fn default_with_state() -> Self {
            Self {
                effects: AsyncMutex::default(),
                submit_started: None,
                release_submit: None,
                cancel_order_error: None,
                position: AsyncMutex::new(Position {
                    instrument: btc_instrument(),
                    qty: 0.0,
                    avg_price: 100.0,
                    unrealized_pnl: 0.0,
                }),
                open_orders: AsyncMutex::new(Vec::new()),
                get_position_calls: AtomicUsize::new(0),
                get_open_orders_calls: AtomicUsize::new(0),
            }
        }

        fn with_blocked_submit(submit_started: Arc<Notify>, release_submit: Arc<Notify>) -> Self {
            Self {
                submit_started: Some(submit_started),
                release_submit: Some(release_submit),
                ..Self::default()
            }
        }

        fn with_cancel_order_error(message: &str) -> Self {
            Self {
                cancel_order_error: Some(message.to_string()),
                ..Self::default()
            }
        }

        async fn set_position_qty(&self, qty: f64) {
            let mut position = self.position.lock().await;
            position.qty = qty;
        }

        fn get_position_calls(&self) -> usize {
            self.get_position_calls.load(Ordering::SeqCst)
        }

        fn get_open_orders_calls(&self) -> usize {
            self.get_open_orders_calls.load(Ordering::SeqCst)
        }
    }

    impl Default for FakeExchange {
        fn default() -> Self {
            Self::default_with_state()
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountSummaryPort for FakeExchange {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
            })
        }
    }

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
            self.effects.lock().await.push(req.clone());
            if let Some(notify) = &self.submit_started {
                notify.notify_waiters();
            }
            if let Some(notify) = &self.release_submit {
                notify.notified().await;
            }
            Ok(OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: req.client_order_id,
                status: OrderStatus::New,
            })
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            if let Some(message) = &self.cancel_order_error {
                return Err(anyhow!(message.clone()));
            }
            Ok(())
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Ok(())
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            self.get_position_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.position.lock().await.clone())
        }

        async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            self.get_open_orders_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.open_orders.lock().await.clone())
        }

        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                instrument: btc_instrument(),
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.1,
                    min_qty: 0.0,
                    min_notional: 0.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            })
        }

        async fn get_account_margin_snapshot(
            &self,
            instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountMarginSnapshot> {
            Ok(poise_engine::ports::AccountMarginSnapshot {
                venue: instrument.venue,
                available_balance: 1_000_000.0,
                total_wallet_balance: 1_000_000.0,
                max_increase_notional: 1_000_000.0,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap())
        }
    }

    #[derive(Default)]
    struct MemoryRepository {
        snapshots: AsyncMutex<HashMap<String, poise_engine::snapshot::TrackRuntimeSnapshot>>,
        effects: AsyncMutex<Vec<PersistedTrackEffect>>,
        follow_up_retirements: AsyncMutex<HashMap<TrackId, Vec<FollowUpRetirementRequest>>>,
        next_effect_batch: AsyncMutex<u64>,
    }

    impl MemoryRepository {
        async fn seed_snapshot(
            &self,
            id: &str,
            snapshot: poise_engine::snapshot::TrackRuntimeSnapshot,
        ) {
            self.snapshots.lock().await.insert(id.to_string(), snapshot);
        }

        async fn seed_effect(&self, effect: PersistedTrackEffect) {
            self.effects.lock().await.push(effect);
        }

        async fn list_all_effects(&self) -> Vec<PersistedTrackEffect> {
            self.effects.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryRepository {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &poise_engine::snapshot::TrackRuntimeSnapshot,
            _events: &[poise_core::events::DomainEvent],
            effects: &[TrackEffect],
            effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());

            let now = Utc::now();
            let mut effect_store = self.effects.lock().await;
            let mut next_effect_batch = self.next_effect_batch.lock().await;
            *next_effect_batch += 1;
            let batch_id = next_effect_batch.to_string();
            let mut persisted_effects = Vec::new();
            for (sequence, effect) in effects.iter().enumerate() {
                if matches!(effect, TrackEffect::NoOp) {
                    continue;
                }

                let persisted = PersistedTrackEffect {
                    effect_id: format!("{id}:{batch_id}:{sequence}"),
                    track_id: TrackId::new(id),
                    batch_id: batch_id.clone(),
                    sequence: u32::try_from(sequence).unwrap(),
                    effect: effect.clone(),
                    status: EffectStatus::Pending,
                    attempt_count: 0,
                    last_error: None,
                    created_at: now,
                    updated_at: now,
                };
                effect_store.push(persisted.clone());
                persisted_effects.push(persisted);
            }

            if let Some(effect_status_update) = effect_status_update {
                let effect = effect_store
                    .iter_mut()
                    .find(|effect| effect.effect_id == effect_status_update.effect_id)
                    .ok_or_else(|| {
                        anyhow!("effect `{}` not found", effect_status_update.effect_id)
                    })?;
                effect.status = effect_status_update.status;
                effect.attempt_count += effect_status_update.attempt_delta;
                effect.last_error = effect_status_update.last_error.clone();
                effect.updated_at = now;
            }

            Ok(CommittedTrackWrite {
                track_id: TrackId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_track_state(
            &self,
            id: &str,
        ) -> Result<Option<poise_engine::snapshot::TrackRuntimeSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect())
        }

        async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn list_pending_submit_effects_for_track_batch(
            &self,
            track_id: &TrackId,
            batch_id: &str,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.batch_id == batch_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn save_follow_up_retirement_request(
            &self,
            track_id: &TrackId,
            request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            let mut stored = self.follow_up_retirements.lock().await;
            let entry = stored.entry(track_id.clone()).or_default();
            if !entry.contains(request) {
                entry.push(request.clone());
            }
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<FollowUpRetirementRequest>> {
            Ok(self
                .follow_up_retirements
                .lock()
                .await
                .get(track_id)
                .cloned()
                .unwrap_or_default())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            track_id: &TrackId,
            request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            let mut stored = self.follow_up_retirements.lock().await;
            if let Some(existing) = stored.get_mut(track_id) {
                existing.retain(|candidate| candidate != request);
                if existing.is_empty() {
                    stored.remove(track_id);
                }
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for MemoryRepository {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .values()
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                })
                .collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .get(track_id.as_str())
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                }))
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .cloned()
                .collect())
        }
    }
}
