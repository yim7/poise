use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use grid_engine::manager::SubmitRecoveryResolution;
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
                .list_pending_effects()
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
                    .effect_service
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
        match self
            .handle_recovered_submit(persisted, &request, target_exposure.clone())
            .await?
        {
            SubmitRecovery::Proceed => {}
            SubmitRecovery::Recovered | SubmitRecovery::AwaitExchangeState => return Ok(()),
        }

        match self.exchange.submit_order(request.clone()).await {
            Ok(receipt) => {
                if let Err(error) = self
                    .state
                    .write_service
                    .record_submit_receipt(
                        persisted.grid_id.as_str(),
                        &request,
                        target_exposure,
                        &receipt,
                    )
                    .await
                {
                    self.state
                        .effect_service
                        .complete_effect_failed(
                            persisted.grid_id.as_str(),
                            &persisted.effect_id,
                            &error.to_string(),
                        )
                        .await?;
                    return Err(error);
                }

                self.state
                    .effect_service
                    .complete_effect_succeeded(persisted.grid_id.as_str(), &persisted.effect_id)
                    .await?;
                Ok(())
            }
            Err(error) => {
                match self
                    .state
                    .write_service
                    .clear_pending_submit(persisted.grid_id.as_str(), &request.client_order_id)
                    .await
                {
                    Ok(()) => {
                        let failure_message = error.to_string();
                        self.state
                            .effect_service
                            .complete_effect_failed(
                                persisted.grid_id.as_str(),
                                &persisted.effect_id,
                                &failure_message,
                            )
                            .await?;
                        Err(anyhow!(failure_message))
                    }
                    Err(clear_error) => Err(anyhow!(
                        "submit order failed: {error}; failed to clear submitting pending order: {clear_error}"
                    )),
                }
            }
        }
    }

    async fn handle_recovered_submit(
        &self,
        persisted: &PersistedGridEffect,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
    ) -> Result<SubmitRecovery> {
        let live_order = self
            .exchange
            .get_open_orders(&request.instrument)
            .await?
            .into_iter()
            .find(|order| order.client_order_id == request.client_order_id);

        match self
            .state
            .write_service
            .recover_submit_effect(
                persisted.grid_id.as_str(),
                &persisted.effect_id,
                request,
                target_exposure,
                live_order.as_ref(),
            )
            .await?
        {
            SubmitRecoveryResolution::Proceed => Ok(SubmitRecovery::Proceed),
            SubmitRecoveryResolution::AwaitExchangeState => Ok(SubmitRecovery::AwaitExchangeState),
            SubmitRecoveryResolution::Succeeded | SubmitRecoveryResolution::Superseded => {
                Ok(SubmitRecovery::Recovered)
            }
        }
    }

    async fn execute_cancellation(
        &self,
        persisted: &PersistedGridEffect,
        cancellation: Cancellation,
    ) -> Result<()> {
        let result = match cancellation {
            Cancellation::One {
                instrument,
                order_id,
            } => self.exchange.cancel_order(&instrument, &order_id).await,
            Cancellation::All { instrument } => self.exchange.cancel_all(&instrument).await,
        };

        match result {
            Ok(()) => {
                self.state
                    .effect_service
                    .complete_effect_succeeded(persisted.grid_id.as_str(), &persisted.effect_id)
                    .await?;
                Ok(())
            }
            Err(error) => {
                self.state
                    .effect_service
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

enum SubmitRecovery {
    Proceed,
    Recovered,
    AwaitExchangeState,
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
    use grid_core::types::ExchangeRules;
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::GridManager;
    use grid_engine::ports::{
        ClockPort, CommittedGridWrite, EffectStatus, EffectStatusUpdate, ExchangeInfo,
        ExchangeOrder, ExchangePort, GridReadRepositoryPort, OrderReceipt, OrderRequest,
        OrderStatus, PersistedGridEffect, Position, StateRepositoryPort, StoredDomainEvent,
        StoredGridSnapshot,
    };
    use grid_engine::runtime::SlotState;
    use grid_engine::transition::GridEffect;
    use tokio::sync::{Mutex as AsyncMutex, broadcast};

    use crate::assembly::build_server_state;
    use crate::effect_service::EffectService;
    use crate::projector::GridProjector;
    use crate::query_service::GridQueryService;
    use crate::write_service::GridWriteService;

    use super::EffectWorker;

    #[tokio::test]
    async fn submit_success_updates_working_order_without_pending_anchor() {
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
            snapshot.pending_order = None;
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
            .as_ref()
            .and_then(|state| state.slots.first())
            .expect("submit receipt should update working order slot");
        assert_eq!(slot.state, SlotState::Working);
        let order = slot
            .working_order
            .as_ref()
            .expect("slot should keep working order after receipt");
        assert_eq!(order.order_id.as_deref(), Some("order-1"));
        assert_eq!(order.status, OrderStatus::New);

        let effect = repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("submit effect should remain persisted");
        assert_eq!(effect.status, EffectStatus::Succeeded);
    }

    async fn test_state(
        repository: Arc<MemoryRepository>,
        exchange: Arc<FakeExchange>,
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
                test_config(),
                test_budget(),
                exchange.get_exchange_info(&instrument).await.unwrap().rules,
            )
            .unwrap();

        let (notifications, _) = broadcast::channel(16);
        let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
        let read_repository: Arc<dyn GridReadRepositoryPort> = repository;
        let effect_service = Arc::new(EffectService::new(
            state_repository.clone(),
            notifications.clone(),
        ));
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

        async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect())
        }

        async fn mark_effect_executing(&self, effect_id: &str) -> Result<()> {
            let mut effects = self.effects.lock().await;
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("effect `{effect_id}` not found"))?;
            effect.status = EffectStatus::Executing;
            effect.updated_at = Utc::now();
            Ok(())
        }

        async fn mark_effect_succeeded(&self, effect_id: &str) -> Result<()> {
            let mut effects = self.effects.lock().await;
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("effect `{effect_id}` not found"))?;
            effect.status = EffectStatus::Succeeded;
            effect.last_error = None;
            effect.updated_at = Utc::now();
            Ok(())
        }

        async fn mark_effect_superseded(&self, effect_id: &str) -> Result<()> {
            let mut effects = self.effects.lock().await;
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("effect `{effect_id}` not found"))?;
            effect.status = EffectStatus::Superseded;
            effect.last_error = None;
            effect.updated_at = Utc::now();
            Ok(())
        }

        async fn mark_effect_failed(&self, effect_id: &str, error: &str) -> Result<()> {
            let mut effects = self.effects.lock().await;
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("effect `{effect_id}` not found"))?;
            effect.status = EffectStatus::Failed;
            effect.attempt_count += 1;
            effect.last_error = Some(error.to_string());
            effect.updated_at = Utc::now();
            Ok(())
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
