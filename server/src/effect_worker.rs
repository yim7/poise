use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use grid_engine::ports::{ExchangePort, OrderRequest, PersistedGridEffect};
use grid_engine::transition::GridEffect;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::assembly::ServerState;

#[derive(Clone)]
pub struct EffectWorker {
    state: ServerState,
    exchange: Arc<dyn ExchangePort>,
    poll_interval: Duration,
}

impl EffectWorker {
    pub fn new(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            state,
            exchange,
            poll_interval,
        }
    }

    pub fn spawn(&self) -> JoinHandle<()> {
        let worker = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = worker.run_once().await {
                    tracing::warn!("effect worker iteration failed: {error}");
                }
                sleep(worker.poll_interval).await;
            }
        })
    }

    pub async fn run_once(&self) -> Result<()> {
        let mut seen_effects = HashSet::new();

        loop {
            let Some(effect) = self
                .state
                .effect_service
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

    async fn process_effect(&self, persisted: PersistedGridEffect) -> Result<()> {
        match persisted.effect {
            GridEffect::SubmitOrder {
                ref request,
                ref target_exposure,
            } => {
                self.execute_submit(&persisted, request.clone(), target_exposure.clone())
                    .await
            }
            GridEffect::CancelOrder {
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
            GridEffect::CancelAll { ref instrument } => {
                self.execute_cancellation(
                    &persisted,
                    Cancellation::All {
                        instrument: instrument.clone(),
                    },
                )
                .await
            }
            GridEffect::NoOp => {
                self.state
                    .write_service
                    .complete_effect_succeeded(persisted.grid_id.as_str(), &persisted.effect_id)
                    .await?;
                Ok(())
            }
        }
    }

    async fn execute_submit(
        &self,
        persisted: &PersistedGridEffect,
        request: OrderRequest,
        target_exposure: grid_core::types::Exposure,
    ) -> Result<()> {
        let Some(prepared_submit) = self
            .prepare_submit_execution(persisted, &request, target_exposure.clone())
            .await?
        else {
            return Ok(());
        };

        match self.exchange.submit_order(request.clone()).await {
            Ok(receipt) => {
                if let Err(error) = self
                    .state
                    .write_service
                    .complete_submit_execution(
                        persisted.grid_id.as_str(),
                        &persisted.effect_id,
                        &request,
                        prepared_submit.target_exposure,
                        &receipt,
                    )
                    .await
                {
                    self.state
                        .write_service
                        .complete_effect_failed(
                            persisted.grid_id.as_str(),
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
                match self
                    .state
                    .write_service
                    .record_submit_failure(
                        persisted.grid_id.as_str(),
                        &persisted.effect_id,
                        &request.client_order_id,
                        &failure_message,
                    )
                    .await
                {
                    Ok(()) => Err(anyhow!(failure_message)),
                    Err(clear_error) => Err(anyhow!(
                        "submit order failed: {error}; failed to record submit failure: {clear_error}"
                    )),
                }
            }
        }
    }

    async fn prepare_submit_execution(
        &self,
        persisted: &PersistedGridEffect,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
    ) -> Result<Option<crate::write_service::PreparedSubmitExecution>> {
        let live_order = self
            .exchange
            .get_open_orders(&request.instrument)
            .await?
            .into_iter()
            .find(|order| order.client_order_id == request.client_order_id);

        self.state
            .write_service
            .prepare_submit_execution(
                persisted.grid_id.as_str(),
                &persisted.effect_id,
                request,
                target_exposure.clone(),
                live_order.as_ref(),
            )
            .await
    }

    async fn execute_cancellation(
        &self,
        persisted: &PersistedGridEffect,
        cancellation: Cancellation,
    ) -> Result<()> {
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
                                persisted.grid_id.as_str(),
                                &persisted.effect_id,
                                order_id,
                            )
                            .await
                    }
                    Cancellation::All { .. } => {
                        self.state
                            .write_service
                            .record_cancel_all_success(
                                persisted.grid_id.as_str(),
                                &persisted.effect_id,
                            )
                            .await
                    }
                };
                if let Err(error) = writeback {
                    self.state
                        .write_service
                        .complete_effect_failed(
                            persisted.grid_id.as_str(),
                            &persisted.effect_id,
                            &error.to_string(),
                        )
                        .await?;
                    return Err(error);
                }
                Ok(())
            }
            Err(error) => {
                self.state
                    .write_service
                    .complete_effect_failed(
                        persisted.grid_id.as_str(),
                        &persisted.effect_id,
                        &error.to_string(),
                    )
                    .await?;
                Err(error)
            }
        }
    }
}

enum Cancellation {
    One {
        instrument: grid_engine::grid::Instrument,
        order_id: String,
    },
    All {
        instrument: grid_engine::grid::Instrument,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{ExchangeRules, Exposure, Side};
    use grid_engine::executor::{ExecutionMode, ExecutionReason, RecoveryAnomaly};
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::GridManager;
    use grid_engine::ports::{
        ClockPort, CommittedGridWrite, EffectStatus, EffectStatusUpdate, ExchangeInfo,
        ExchangeOrder, ExchangePort, GridReadRepositoryPort, OrderReceipt, OrderRequest,
        OrderStatus, PersistedGridEffect, Position, StateRepositoryPort, StoredDomainEvent,
        StoredGridSnapshot,
    };
    use grid_engine::runtime::{
        ExecutionStats, ExecutorState, GridStatus, RiskState, SlotState, WorkingOrder,
    };
    use grid_engine::snapshot::{GridRuntimeSnapshot, ObservedState};
    use grid_engine::transition::GridEffect;
    use tokio::sync::{Mutex as AsyncMutex, broadcast};

    use crate::assembly::build_server_state;
    use crate::effect_service::EffectService;
    use crate::projector::GridProjector;
    use crate::query_service::GridQueryService;
    use crate::write_service::GridWriteService;

    use super::EffectWorker;

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
            [GridEffect::SubmitOrder { .. }]
        ));

        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.snapshot("btc-core").unwrap();
            snapshot
                .executor_state
                .slots
                .push(grid_engine::runtime::ExecutionSlot {
                    slot: grid_engine::executor::OrderSlot::new("inventory_followup"),
                    state: SlotState::Working,
                    working_order: Some(grid_engine::runtime::WorkingOrder {
                        order_id: Some("order-2".into()),
                        client_order_id: "client-2".into(),
                        side: Side::Sell,
                        price: 96.0,
                        quantity: 0.1,
                        target_exposure: Exposure(2.0),
                        status: OrderStatus::PartiallyFilled,
                        role: grid_engine::executor::OrderRole::DecreaseInventory,
                    }),
                });
            manager.restore_grid_state(&snapshot).unwrap();
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
            grid_engine::executor::OrderSlot::new("inventory_followup")
        );
        assert_eq!(
            snapshot.executor_state.slots[1]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-2")
        );

        let effect = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("submit effect should remain persisted");
        assert_eq!(effect.status, EffectStatus::Succeeded);
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
            .seed_effect(PersistedGridEffect {
                effect_id: "btc-core:batch:0".into(),
                grid_id: GridId::new("btc-core"),
                batch_id: "batch".into(),
                sequence: 0,
                effect: GridEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "BTCUSDT-reconcile".into(),
                    },
                    target_exposure: Exposure(6.0),
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
            manager.restore_grid_state(&snapshot).unwrap();
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
            manager.restore_grid_state(&snapshot).unwrap();
        }
        repository
            .seed_effect(PersistedGridEffect {
                effect_id: "btc-core:batch:0".into(),
                grid_id: GridId::new("btc-core"),
                batch_id: "batch".into(),
                sequence: 0,
                effect: GridEffect::CancelOrder {
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
        assert!(snapshot.executor_state.slots.is_empty());

        let effect = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("cancel effect should remain persisted");
        assert_eq!(effect.status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn submit_recovery_proceed_receipt_keeps_current_plan_target() {
        let repository = Arc::new(MemoryRepository::default());
        let exchange = Arc::new(FakeExchange::default());
        let config = GridConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        };
        let exchange_rules = ExchangeRules {
            price_tick: 10.0,
            quantity_step: 1.0,
            min_qty: 0.0,
            min_notional: 0.0,
        };
        let state = test_state_with_grid(
            repository.clone(),
            exchange.clone(),
            config.clone(),
            exchange_rules,
        )
        .await;
        let expected_target = grid_core::strategy::target_exposure(94.99, &config);
        let snapshot = snapshot_with_submit_pending_order(
            94.99,
            config.clone(),
            WorkingOrder {
                order_id: None,
                client_order_id: "btc-core-reconcile".into(),
                side: Side::Buy,
                price: 90.0,
                quantity: 4.0,
                target_exposure: Exposure(4.0),
                status: OrderStatus::Submitting,
                role: grid_engine::executor::OrderRole::IncreaseInventory,
            },
        );

        repository.seed_snapshot("btc-core", snapshot.clone()).await;
        {
            let manager_handle = state.write_service.manager();
            let mut manager = manager_handle.write().await;
            manager.restore_grid_state(&snapshot).unwrap();
        }
        repository
            .seed_effect(PersistedGridEffect {
                effect_id: "btc-core:batch:0".into(),
                grid_id: GridId::new("btc-core"),
                batch_id: "batch".into(),
                sequence: 0,
                effect: GridEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 90.0,
                        quantity: 4.0,
                        client_order_id: "btc-core-reconcile".into(),
                    },
                    target_exposure: Exposure(4.0),
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
                .slots
                .first()
                .and_then(|slot| slot.working_order.as_ref())
                .map(|order| order.target_exposure.clone()),
            Some(expected_target)
        );
    }

    async fn test_state(
        repository: Arc<MemoryRepository>,
        exchange: Arc<FakeExchange>,
    ) -> crate::assembly::ServerState {
        test_state_with_grid(
            repository,
            exchange,
            test_config(),
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.1,
                min_qty: 0.0,
                min_notional: 0.0,
            },
        )
        .await
    }

    async fn test_state_with_grid(
        repository: Arc<MemoryRepository>,
        _exchange: Arc<FakeExchange>,
        config: GridConfig,
        exchange_rules: ExchangeRules,
    ) -> crate::assembly::ServerState {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = GridManager::new(clock);
        let instrument = btc_instrument();
        manager
            .add_grid(
                GridId::new("btc-core"),
                instrument.clone(),
                config,
                test_budget(),
                exchange_rules,
            )
            .unwrap();

        let (notifications, _) = broadcast::channel(16);
        let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
        let read_repository: Arc<dyn GridReadRepositoryPort> = repository;
        let effect_service = Arc::new(EffectService::new(state_repository.clone()));
        let write_service = Arc::new(GridWriteService::new(
            manager,
            state_repository,
            notifications.clone(),
        ));
        build_server_state(
            write_service,
            effect_service,
            Arc::new(GridQueryService::new(read_repository)),
            Arc::new(GridProjector::new()),
        )
    }

    fn btc_instrument() -> Instrument {
        Instrument::new(Venue::Binance, "BTCUSDT")
    }

    fn snapshot_with_recovery_anomaly() -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: GridId::new("btc-core"),
            instrument: btc_instrument(),
            config: test_config(),
            status: GridStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(6.0)),
            executor_state: ExecutorState {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(6.0),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                slots: vec![],
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: Some(RecoveryAnomaly::UnknownLiveOrder),
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
            },
        }
    }

    fn snapshot_with_working_order() -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: GridId::new("btc-core"),
            instrument: btc_instrument(),
            config: test_config(),
            status: GridStatus::Active,
            current_exposure: Exposure(2.0),
            target_exposure: Some(Exposure(6.0)),
            executor_state: ExecutorState {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(4.0),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                slots: vec![grid_engine::runtime::ExecutionSlot {
                    slot: grid_engine::executor::OrderSlot::new("inventory_core"),
                    state: SlotState::Working,
                    working_order: Some(grid_engine::runtime::WorkingOrder {
                        order_id: Some("order-1".into()),
                        client_order_id: "client-1".into(),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: 15.0,
                        target_exposure: Exposure(6.0),
                        status: OrderStatus::New,
                        role: grid_engine::executor::OrderRole::IncreaseInventory,
                    }),
                }],
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
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
            },
        }
    }

    fn snapshot_with_submit_pending_order(
        reference_price: f64,
        config: GridConfig,
        order: WorkingOrder,
    ) -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: GridId::new("btc-core"),
            instrument: btc_instrument(),
            config: config.clone(),
            status: GridStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(grid_core::strategy::target_exposure(
                reference_price,
                &config,
            )),
            executor_state: ExecutorState {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(order.target_exposure.0),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                slots: vec![grid_engine::runtime::ExecutionSlot {
                    slot: grid_engine::executor::OrderSlot::new("inventory_core"),
                    state: SlotState::SubmitPending,
                    working_order: Some(order),
                }],
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
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
            },
        }
    }

    fn test_config() -> GridConfig {
        GridConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
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

    #[derive(Default)]
    struct FakeExchange {
        effects: AsyncMutex<Vec<OrderRequest>>,
    }

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
            self.effects.lock().await.push(req.clone());
            Ok(OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: req.client_order_id,
                status: OrderStatus::New,
            })
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Ok(())
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            Ok(Position {
                instrument: btc_instrument(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            Ok(Vec::new())
        }

        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                instrument: btc_instrument(),
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.1,
                    min_qty: 0.0,
                    min_notional: 0.0,
                },
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap())
        }
    }

    #[derive(Default)]
    struct MemoryRepository {
        snapshots: AsyncMutex<HashMap<String, grid_engine::snapshot::GridRuntimeSnapshot>>,
        effects: AsyncMutex<Vec<PersistedGridEffect>>,
        next_effect_batch: AsyncMutex<u64>,
    }

    impl MemoryRepository {
        async fn seed_snapshot(
            &self,
            id: &str,
            snapshot: grid_engine::snapshot::GridRuntimeSnapshot,
        ) {
            self.snapshots.lock().await.insert(id.to_string(), snapshot);
        }

        async fn seed_effect(&self, effect: PersistedGridEffect) {
            self.effects.lock().await.push(effect);
        }

        async fn list_all_effects(&self) -> Vec<PersistedGridEffect> {
            self.effects.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryRepository {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &grid_engine::snapshot::GridRuntimeSnapshot,
            _events: &[grid_core::events::DomainEvent],
            effects: &[GridEffect],
            effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedGridWrite> {
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
                if matches!(effect, GridEffect::NoOp) {
                    continue;
                }

                let persisted = PersistedGridEffect {
                    effect_id: format!("{id}:{batch_id}:{sequence}"),
                    grid_id: GridId::new(id),
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

            Ok(CommittedGridWrite {
                grid_id: GridId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_grid_state(
            &self,
            id: &str,
        ) -> Result<Option<grid_engine::snapshot::GridRuntimeSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect())
        }

        async fn list_pending_submit_effects_for_grid(
            &self,
            grid_id: &GridId,
        ) -> Result<Vec<PersistedGridEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.grid_id == *grid_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, GridEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }
    }

    #[async_trait::async_trait]
    impl GridReadRepositoryPort for MemoryRepository {
        async fn list_grid_snapshots(&self) -> Result<Vec<StoredGridSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .values()
                .cloned()
                .map(|snapshot| StoredGridSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                })
                .collect())
        }

        async fn load_grid_snapshot(&self, grid_id: &GridId) -> Result<Option<StoredGridSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .get(grid_id.as_str())
                .cloned()
                .map(|snapshot| StoredGridSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                }))
        }

        async fn list_recent_grid_events(
            &self,
            _grid_id: &GridId,
            _limit: usize,
        ) -> Result<Vec<StoredDomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_grid_effects(
            &self,
            grid_id: &GridId,
            _limit: usize,
        ) -> Result<Vec<PersistedGridEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.grid_id == *grid_id)
                .cloned()
                .collect())
        }
    }
}
