use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use grid_engine::command::GridCommand;
use grid_engine::observation::{OrderObservation, PositionObservation};
use grid_engine::ports::{
    ExchangeOrder, ExchangePort, MarketDataPort, OrderStatus, Position, UserDataEvent,
    UserDataPayload,
};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use crate::assembly::ServerState;
use crate::effect_worker::EffectWorker;
use crate::write_service::GridMutationError;
#[derive(Clone)]
pub struct ServerRuntime {
    state: ServerState,
    exchange: Arc<dyn ExchangePort>,
    market_data: Arc<dyn MarketDataPort>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub struct RuntimeHandles {
    #[cfg_attr(not(test), allow(dead_code))]
    pub market_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub user_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub effect_task: JoinHandle<()>,
}

impl ServerRuntime {
    pub fn new(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        market_data: Arc<dyn MarketDataPort>,
    ) -> Self {
        Self {
            state,
            exchange,
            market_data,
        }
    }

    pub async fn start(&self) -> Result<RuntimeHandles> {
        let mut user_receiver = self.market_data.subscribe_user_data().await?;
        let startup_cutoff = self.exchange.get_server_time().await?;
        self.startup_sync().await?;
        self.replay_startup_user_data(&mut user_receiver, startup_cutoff)
            .await?;
        let effect_task = self.spawn_effect_task();
        let user_task = self.spawn_user_task(user_receiver, startup_cutoff);
        let market_task = self.spawn_market_task();

        Ok(RuntimeHandles {
            market_task,
            user_task,
            effect_task,
        })
    }

    async fn startup_sync(&self) -> Result<()> {
        let repository = self.state.write_service.repository();
        for grid in self.state.write_service.grid_instruments().await {
            let position = self.exchange.get_position(&grid.instrument).await?;
            let open_orders = self.exchange.get_open_orders(&grid.instrument).await?;
            let preserve_submitting_anchor = repository
                .load_grid_state(&grid.id)
                .await?
                .and_then(|snapshot| snapshot.pending_order)
                .map(|pending| pending.status == OrderStatus::Submitting)
                .unwrap_or(false);

            if !preserve_submitting_anchor {
                self.state
                    .write_service
                    .clear_pending_order(&grid.id)
                    .await?;
            }
            let _ = self
                .state
                .write_service
                .observe_position(&grid.id, position_observation(&position))
                .await?;
            for order in &open_orders {
                let _ = self
                    .state
                    .write_service
                    .observe_order(&grid.id, order_observation(order))
                    .await?;
            }
        }

        Ok(())
    }

    async fn replay_startup_user_data(
        &self,
        receiver: &mut mpsc::Receiver<UserDataEvent>,
        startup_cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let mut buffered_events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            buffered_events.push(event);
        }

        buffered_events.sort_by_key(|event| event.event_time);
        for event in buffered_events {
            if event.event_time > startup_cutoff {
                let should_reconcile = should_reconcile_after_user_data(&event);
                let instrument = event.instrument().clone();
                let Some(grid_id) = self.state.write_service.resolve_grid_id(&instrument).await
                else {
                    tracing::warn!(
                        "received user data for unknown instrument {}:{}",
                        instrument.venue.as_str(),
                        instrument.symbol
                    );
                    continue;
                };
                apply_user_data_event(&self.state, &grid_id, event)
                    .await
                    .map_err(mutate_error)?;
                if should_reconcile {
                    command_reconcile(&self.state, &grid_id).await?;
                }
            }
        }

        Ok(())
    }

    fn spawn_market_task(&self) -> JoinHandle<()> {
        let state = self.state.clone();
        let market_data = Arc::clone(&self.market_data);

        tokio::spawn(async move {
            let grids = state.write_service.grid_instruments().await;
            let mut workers = JoinSet::new();

            for grid in grids {
                let instrument = grid.instrument.clone();
                match market_data.subscribe_prices(&instrument).await {
                    Ok(mut receiver) => {
                        let state = state.clone();
                        workers.spawn(async move {
                            while let Some(tick) = receiver.recv().await {
                                match state
                                    .write_service
                                    .observe_market(&grid.id, tick.reference_price)
                                    .await
                                {
                                    Ok(_) => {}
                                    Err(error) => {
                                        tracing::warn!(
                                            "failed to apply market data update for {}: {}",
                                            instrument.symbol,
                                            error
                                        );
                                    }
                                }
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(
                            "failed to subscribe market data for {}: {error}",
                            instrument.symbol
                        );
                    }
                }
            }

            while let Some(result) = workers.join_next().await {
                if let Err(error) = result {
                    tracing::warn!("market worker join error: {error}");
                }
            }
        })
    }

    fn spawn_effect_task(&self) -> JoinHandle<()> {
        EffectWorker::new(
            self.state.clone(),
            Arc::clone(&self.exchange),
            Duration::from_millis(10),
        )
        .spawn()
    }

    fn spawn_user_task(
        &self,
        mut receiver: mpsc::Receiver<UserDataEvent>,
        startup_cutoff: chrono::DateTime<chrono::Utc>,
    ) -> JoinHandle<()> {
        let state = self.state.clone();

        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                if event.event_time <= startup_cutoff {
                    continue;
                }

                let should_reconcile = should_reconcile_after_user_data(&event);
                let instrument = event.instrument().clone();
                let Some(grid_id) = state.write_service.resolve_grid_id(&instrument).await else {
                    tracing::warn!(
                        "received user data for unknown instrument {}:{}",
                        instrument.venue.as_str(),
                        instrument.symbol
                    );
                    continue;
                };
                if let Err(error) = apply_user_data_event(&state, &grid_id, event).await {
                    tracing::warn!(
                        "failed to apply user data update for {}: {}",
                        instrument.symbol,
                        error.message()
                    );
                    continue;
                }

                if should_reconcile && let Err(error) = command_reconcile(&state, &grid_id).await {
                    tracing::warn!(
                        "failed to reconcile after user data update for {}: {error}",
                        instrument.symbol
                    );
                }
            }
        })
    }
}

fn should_reconcile_after_user_data(event: &UserDataEvent) -> bool {
    match &event.payload {
        UserDataPayload::PositionUpdate(_) => true,
        // FILLED / PARTIALLY_FILLED 需要等待随后到达的仓位更新提供真实 exposure，
        // 否则会在旧仓位上提前补单。撤单/拒单/过期不会改仓位，可以立刻重算。
        UserDataPayload::OrderUpdate(order) => order.status.should_reconcile_after_order_update(),
    }
}

async fn apply_user_data_event(
    state: &ServerState,
    grid_id: &str,
    event: UserDataEvent,
) -> std::result::Result<(), GridMutationError> {
    match event.payload {
        UserDataPayload::PositionUpdate(position) => {
            let _ = state
                .write_service
                .observe_position(grid_id, position_observation(&position))
                .await
                .map_err(preserve_grid_mutation_error)?;
        }
        UserDataPayload::OrderUpdate(order) => {
            let _ = state
                .write_service
                .observe_order(grid_id, order_observation(&order))
                .await
                .map_err(preserve_grid_mutation_error)?;
        }
    }

    Ok(())
}

fn preserve_grid_mutation_error(error: anyhow::Error) -> GridMutationError {
    match error.downcast::<GridMutationError>() {
        Ok(error) => error,
        Err(other) => GridMutationError::Persistence(other),
    }
}

async fn command_reconcile(state: &ServerState, grid_id: &str) -> Result<()> {
    let _ = state
        .write_service
        .command(grid_id, GridCommand::Reconcile)
        .await?;
    Ok(())
}

fn mutate_error(error: GridMutationError) -> anyhow::Error {
    anyhow!(error.message())
}

fn position_observation(position: &Position) -> PositionObservation {
    PositionObservation {
        qty: position.qty,
        unrealized_pnl: position.unrealized_pnl,
    }
}

fn order_observation(order: &ExchangeOrder) -> OrderObservation {
    OrderObservation {
        order_id: order.order_id.clone(),
        client_order_id: order.client_order_id.clone(),
        side: order.side,
        price: order.price,
        quantity: order.qty,
        realized_pnl: order.realized_pnl,
        status: order.status,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{ExchangeRules, Exposure, Side};
    use grid_engine::command::GridCommand;
    use grid_engine::execution_plan::ExecutionAction;
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::GridManager;
    use grid_engine::ports::{
        ClockPort, CommittedGridWrite, EffectStatus, ExchangeInfo, ExchangeOrder, ExchangePort,
        GridReadRepositoryPort, GridSnapshot, MarketDataPort, OrderReceipt, OrderRequest,
        OrderStatus, PersistedGridEffect, Position, PriceTick, StateRepositoryPort,
        StoredDomainEvent, StoredGridSnapshot, UserDataEvent, UserDataPayload,
    };
    use grid_engine::runtime::{GridRuntime, GridStatus, PendingOrder, RiskState};
    use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};
    use tokio::time::{sleep, timeout};

    use crate::assembly::{ServerState, build_server_state};
    use crate::effect_worker::EffectWorker;
    use crate::projector::GridProjector;
    use crate::query_service::GridQueryService;
    use crate::write_service::GridWriteService;

    use super::{RuntimeHandles, ServerRuntime};

    #[tokio::test]
    async fn market_tick_submits_order_and_records_pending_order() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_instance(&fixture.state, |instance| instance.pending_order.is_some()).await;

        let instance = current_instance(&fixture.state).await;
        let pending = instance.pending_order.unwrap();
        assert_eq!(pending.order_id.as_deref(), Some("order-1"));
        assert_eq!(pending.target_exposure, Exposure(4.0));

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn effect_worker_executes_persisted_submit_order_and_marks_success() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let transition = fixture
            .state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );
        assert_eq!(
            fixture
                .persistence
                .list_pending_effects()
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        let handles = fixture.runtime.start().await.unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_async(|| {
            let persistence = Arc::clone(&fixture.persistence);
            async move { persistence.list_pending_effects().await.unwrap().is_empty() }
        })
        .await;

        let instance = current_instance(&fixture.state).await;
        assert_eq!(
            instance
                .pending_order
                .as_ref()
                .and_then(|pending| pending.order_id.as_deref()),
            Some("order-1")
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn effect_worker_restores_pending_effect_after_restart() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        fixture
            .state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .persistence
                .list_pending_effects()
                .await
                .unwrap()
                .len(),
            1
        );

        let (_price_sender, price_receiver) = mpsc::channel(8);
        let (_user_sender, user_receiver) = mpsc::channel(8);
        let restarted_runtime = ServerRuntime::new(
            fixture.state.clone(),
            fixture.exchange.clone() as Arc<dyn ExchangePort>,
            Arc::new(FakeMarketData::new(price_receiver, user_receiver)) as Arc<dyn MarketDataPort>,
        );

        let handles = restarted_runtime.start().await.unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_async(|| {
            let persistence = Arc::clone(&fixture.persistence);
            async move { persistence.list_pending_effects().await.unwrap().is_empty() }
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn failed_effect_does_not_roll_back_committed_snapshot() {
        let exchange = Arc::new(FakeExchange::with_submit_error(
            btc_position(0.0, 0.0),
            vec![],
            "submit rejected",
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let (_price_sender, price_receiver) = mpsc::channel(8);
        let (_user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::new(price_receiver, user_receiver));
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let runtime = ServerRuntime::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );
        assert_eq!(persistence.list_pending_effects().await.unwrap().len(), 1);

        let handles = runtime.start().await.unwrap();

        wait_until_async(|| {
            let persistence = Arc::clone(&persistence);
            async move {
                persistence
                    .all_effects()
                    .await
                    .iter()
                    .any(|effect| effect.status == EffectStatus::Failed)
            }
        })
        .await;

        let instance = current_instance(&state).await;
        assert_eq!(instance.target_exposure, Some(Exposure(4.0)));
        assert!(instance.pending_order.is_none());

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn effect_worker_leaves_submitting_pending_order_when_receipt_persistence_fails() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(FailOnReceiptPersistence::default());
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );
        worker.run_once().await.unwrap();

        let instance = current_instance(&state).await;
        let pending = instance
            .pending_order
            .expect("submit intent should remain durable");
        assert_eq!(pending.order_id, None);
        assert_eq!(pending.status, OrderStatus::Submitting);

        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Failed);
    }

    #[tokio::test]
    async fn effect_worker_keeps_action_target_when_instance_changes_before_receipt_recording() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let outcome = fixture
            .state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        let target_exposure = match outcome.effects.as_slice() {
            [
                ExecutionAction::SubmitOrder {
                    target_exposure, ..
                },
            ] => target_exposure.clone(),
            other => panic!("unexpected actions: {other:?}"),
        };

        fixture
            .state
            .write_service
            .command("BTCUSDT", GridCommand::Pause)
            .await
            .unwrap();
        let handles = fixture.runtime.start().await.unwrap();
        wait_until_instance(&fixture.state, |instance| instance.pending_order.is_some()).await;

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.target_exposure, None);
        assert_eq!(
            instance
                .pending_order
                .map(|pending| pending.target_exposure),
            Some(target_exposure)
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn effect_worker_does_not_resubmit_when_matching_pending_order_is_already_restored() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        let (request, target_exposure) = match transition.effects.as_slice() {
            [
                ExecutionAction::SubmitOrder {
                    request,
                    target_exposure,
                },
            ] => (request.clone(), target_exposure.clone()),
            other => panic!("unexpected actions: {other:?}"),
        };

        state
            .write_service
            .record_pending_order(
                "BTCUSDT",
                PendingOrder {
                    order_id: Some("order-restored".into()),
                    client_order_id: request.client_order_id.clone(),
                    side: request.side,
                    price: request.price,
                    quantity: request.quantity,
                    target_exposure,
                    status: OrderStatus::New,
                },
            )
            .await
            .unwrap();

        worker.run_once().await.unwrap();

        assert!(
            exchange.submitted_orders.lock().unwrap().is_empty(),
            "worker should not resubmit when the same client order is already restored"
        );
        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn effect_worker_does_not_submit_follow_up_effect_after_failed_cancel_in_same_batch() {
        let exchange = Arc::new(FakeExchange::with_cancel_all_error(
            btc_position(0.0, 0.0),
            vec![],
            "cancel rejected",
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(4.0));
        snapshot.pending_order = Some(PendingOrder {
            order_id: Some("snapshot-1".into()),
            client_order_id: "snapshot-1".into(),
            side: Side::Buy,
            price: 94.0,
            quantity: 0.25,
            target_exposure: Exposure(4.0),
            status: OrderStatus::New,
        });
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(snapshot),
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 90.0)
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [
                ExecutionAction::CancelAll { .. },
                ExecutionAction::SubmitOrder { .. }
            ]
        ));

        worker.run_once().await.unwrap();

        assert_eq!(
            exchange.cancel_all_symbols.lock().unwrap().as_slice(),
            ["BTCUSDT"]
        );
        assert!(
            exchange.submitted_orders.lock().unwrap().is_empty(),
            "submit should stay blocked behind failed cancel"
        );

        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].status, EffectStatus::Failed);
        assert_eq!(effects[1].status, EffectStatus::Pending);
    }

    #[tokio::test]
    async fn effect_worker_marks_effect_failed_even_if_submit_cleanup_persistence_fails() {
        let exchange = Arc::new(FakeExchange::with_submit_error(
            btc_position(0.0, 0.0),
            vec![],
            "submit rejected",
        ));
        let persistence = Arc::new(FailOnSavePersistence::new(3));
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );

        worker.run_once().await.unwrap();

        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Failed);
        assert_eq!(effects[0].attempt_count, 1);

        let instance = current_instance(&state).await;
        assert_eq!(
            instance
                .pending_order
                .as_ref()
                .map(|pending| pending.status),
            Some(OrderStatus::Submitting)
        );
    }

    #[tokio::test]
    async fn effect_worker_keeps_effect_pending_while_submit_is_inflight() {
        let submit_started = Arc::new(Notify::new());
        let release_submit = Arc::new(Notify::new());
        let exchange = Arc::new(FakeExchange::with_blocked_submit(
            btc_position(0.0, 0.0),
            vec![],
            submit_started.clone(),
            release_submit.clone(),
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();

        let task = tokio::spawn({
            let worker = worker.clone();
            async move { worker.run_once().await }
        });

        submit_started.notified().await;
        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Pending);

        release_submit.notify_waiters();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn position_update_reconciles_actual_exposure_without_overwriting_target() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
        wait_until_instance(&fixture.state, |instance| {
            instance
                .target_exposure
                .as_ref()
                .map(|exposure| (exposure.0 - 4.0).abs() < f64::EPSILON)
                .unwrap_or(false)
        })
        .await;

        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                11.0,
            ))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| {
            (instance.current_exposure.0 - 2.0).abs() < f64::EPSILON
                && instance
                    .target_exposure
                    .as_ref()
                    .map(|exposure| (exposure.0 - 4.0).abs() < f64::EPSILON)
                    .unwrap_or(false)
        })
        .await;

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert_eq!(instance.target_exposure, Some(Exposure(4.0)));
        assert!((instance.risk_state.unrealized_pnl - 11.0).abs() < f64::EPSILON);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn position_update_submits_reconcile_without_waiting_for_new_tick() {
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(4.0));
        snapshot.pending_order = None;
        snapshot.observed.reference_price = Some(95.0);

        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                11.0,
            ))
            .await
            .unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_instance(&fixture.state, |instance| {
            instance
                .pending_order
                .as_ref()
                .and_then(|pending| pending.order_id.as_deref())
                == Some("order-1")
        })
        .await;

        let submitted = fixture.exchange.submitted_orders.lock().unwrap().clone();
        assert_eq!(submitted[0].side, Side::Buy);
        assert_eq!(submitted[0].quantity, 7.5);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn position_update_broadcasts_snapshot_updated_when_reconcile_emits_no_domain_event() {
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(0.0));
        snapshot.pending_order = None;
        snapshot.observed.reference_price = Some(100.0);
        snapshot.risk.unrealized_pnl = 0.0;

        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        let mut receiver = fixture.state.write_service.subscribe_notifications();
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                0.0,
                11.0,
            ))
            .await
            .unwrap();

        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            crate::notifications::GridInternalNotification::GridWriteCommitted { .. }
        ));

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn order_update_clears_pending_order_on_terminal_status() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
        wait_until_instance(&fixture.state, |instance| instance.pending_order.is_some()).await;

        let pending = current_instance(&fixture.state)
            .await
            .pending_order
            .unwrap();

        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    &pending.order_id.clone().unwrap(),
                    &pending.client_order_id,
                    Side::Buy,
                    pending.price,
                    pending.quantity,
                    0.0,
                    OrderStatus::Filled,
                ),
            ))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| instance.pending_order.is_none()).await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn terminal_order_update_reconciles_without_waiting_for_new_tick() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;

        let pending = current_instance(&fixture.state)
            .await
            .pending_order
            .unwrap();

        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    &pending.order_id.clone().unwrap(),
                    &pending.client_order_id,
                    Side::Buy,
                    pending.price,
                    pending.quantity,
                    0.0,
                    OrderStatus::Canceled,
                ),
            ))
            .await
            .unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 2).await;
        wait_until_instance(&fixture.state, |instance| {
            instance
                .pending_order
                .as_ref()
                .and_then(|pending| pending.order_id.as_deref())
                == Some("order-2")
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn terminal_order_update_broadcasts_snapshot_updated_when_reconcile_emits_no_domain_event()
     {
        let snapshot = GridSnapshot {
            grid_id: GridId::new("BTCUSDT"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            config: test_config(),
            status: GridStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(0.0)),
            pending_order: Some(PendingOrder {
                order_id: Some("order-1".into()),
                client_order_id: "order-1".into(),
                side: Side::Buy,
                price: 100.0,
                quantity: 0.1,
                target_exposure: Exposure(0.0),
                status: OrderStatus::New,
            }),
            risk: RiskState::default(),
            observed: grid_engine::snapshot::ObservedState {
                reference_price: Some(100.0),
                out_of_band_since: None,
            },
        };
        let open_orders = vec![ExchangeOrder {
            instrument: btc_instrument(),
            order_id: "order-1".into(),
            client_order_id: "order-1".into(),
            side: Side::Buy,
            price: 100.0,
            qty: 0.1,
            realized_pnl: 0.0,
            status: OrderStatus::New,
        }];
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(0.0, 0.0),
            open_orders,
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        let mut receiver = fixture.state.write_service.subscribe_notifications();
        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    "order-1",
                    "order-1",
                    Side::Buy,
                    100.0,
                    0.1,
                    0.0,
                    OrderStatus::Canceled,
                ),
            ))
            .await
            .unwrap();

        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            crate::notifications::GridInternalNotification::GridWriteCommitted { .. }
        ));

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_uses_live_position_and_open_orders_before_first_tick() {
        let snapshot = test_snapshot();
        let live_order = btc_exchange_order(
            "live-1",
            "live-1",
            Side::Buy,
            94.5,
            0.25,
            0.0,
            OrderStatus::New,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(7.5, 3.0),
            vec![live_order],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert_eq!(instance.target_exposure, Some(Exposure(6.0)));
        assert_eq!(
            instance.out_of_band_since,
            Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap())
        );
        assert_eq!(
            instance
                .pending_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("live-1")
        );
        assert_eq!(
            instance
                .pending_order
                .as_ref()
                .map(|order| order.target_exposure.clone()),
            Some(Exposure(6.0))
        );

        fixture.price_sender.send(btc_tick(92.5)).await.unwrap();
        sleep(Duration::from_millis(100)).await;

        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_clears_stale_pending_order_when_exchange_has_no_open_orders() {
        let fixture = runtime_fixture(
            Some(test_snapshot()),
            btc_position(7.5, 3.0),
            vec![],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert_eq!(instance.target_exposure, Some(Exposure(6.0)));
        assert_eq!(instance.pending_order, None);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_preserves_submitting_pending_order_until_exchange_catches_up() {
        let mut snapshot = test_snapshot();
        snapshot.pending_order = Some(PendingOrder {
            order_id: None,
            client_order_id: "BTCUSDT-reconcile".into(),
            side: Side::Buy,
            price: 94.0,
            quantity: 0.25,
            target_exposure: Exposure(6.0),
            status: OrderStatus::Submitting,
        });
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(7.5, 3.0),
            vec![],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(
            instance
                .pending_order
                .as_ref()
                .map(|pending| pending.status),
            Some(OrderStatus::Submitting)
        );

        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
        sleep(Duration::from_millis(100)).await;

        assert!(
            fixture.exchange.submitted_orders.lock().unwrap().is_empty(),
            "submitting recovery anchor should suppress duplicate submit before exchange state arrives"
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_replays_buffered_user_event_before_first_tick() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                5.0,
            ))
            .await
            .unwrap();

        let handles = fixture.runtime.start().await.unwrap();

        wait_until_instance(&fixture.state, |instance| {
            (instance.current_exposure.0 - 2.0).abs() < f64::EPSILON
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_ignores_buffered_user_event_older_than_cutoff() {
        let fixture = runtime_fixture(None, btc_position(7.5, 3.0), vec![], test_budget()).await;
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() - chrono::Duration::milliseconds(1),
                3.75,
                9.0,
            ))
            .await
            .unwrap();

        let handles = fixture.runtime.start().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert!((instance.risk_state.unrealized_pnl - 3.0).abs() < f64::EPSILON);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn runtime_start_fails_when_buffered_user_data_replay_cannot_be_persisted() {
        let (price_sender, price_receiver) = mpsc::channel(8);
        drop(price_sender);
        let (user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::new(price_receiver, user_receiver));
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(FailOnSavePersistence::new(2));
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = GridManager::new(clock);
        manager
            .add_grid(
                GridId::new("BTCUSDT"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let write_service = Arc::new(GridWriteService::new(
            manager,
            persistence.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            Arc::new(GridQueryService::new(
                persistence.clone() as Arc<dyn GridReadRepositoryPort>
            )),
            Arc::new(GridProjector::new()),
        );
        let runtime = ServerRuntime::new(
            state,
            exchange as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );
        user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                5.0,
            ))
            .await
            .unwrap();

        let error = runtime.start().await.err().unwrap();
        assert!(error.to_string().contains("injected save failure"));
    }

    #[tokio::test]
    async fn apply_user_data_event_preserves_write_service_mutation_error_kind() {
        let manager = GridManager::new(Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        )));
        let persistence = Arc::new(MemoryPersistence::default());
        let (events, _) = broadcast::channel(16);
        let state = build_server_state(
            Arc::new(GridWriteService::new(
                manager,
                persistence.clone() as Arc<dyn StateRepositoryPort>,
                events,
            )),
            Arc::new(GridQueryService::new(
                persistence as Arc<dyn GridReadRepositoryPort>,
            )),
            Arc::new(GridProjector::new()),
        );

        let error = super::apply_user_data_event(
            &state,
            "missing-grid",
            position_event_at(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 1).unwrap(),
                1.0,
                0.0,
            ),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            crate::write_service::GridMutationError::Mutation(_)
        ));
    }

    #[tokio::test]
    async fn stale_live_user_event_does_not_rollback_state_after_start() {
        let fixture = runtime_fixture(None, btc_position(7.5, 3.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() - chrono::Duration::milliseconds(1),
                3.75,
                9.0,
            ))
            .await
            .unwrap();
        sleep(Duration::from_millis(100)).await;

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert!((instance.risk_state.unrealized_pnl - 3.0).abs() < f64::EPSILON);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn filled_order_updates_realized_pnl_and_trips_daily_loss_cap() {
        let fixture = runtime_fixture(
            None,
            btc_position(7.5, 0.0),
            vec![],
            CapacityBudget {
                max_notional: 3000.0,
                daily_loss_limit: -10.0,
                stop_loss_pct: 10.0,
            },
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    "fill-1",
                    "fill-1",
                    Side::Sell,
                    95.0,
                    7.5,
                    -20.0,
                    OrderStatus::Filled,
                ),
            ))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| {
            (instance.risk_state.realized_pnl_today + 20.0).abs() < f64::EPSILON
        })
        .await;

        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;

        let submitted = fixture.exchange.submitted_orders.lock().unwrap().clone();
        assert_eq!(submitted[0].side, Side::Sell);
        assert_eq!(
            current_instance(&fixture.state).await.target_exposure,
            Some(Exposure(0.0))
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn runtime_start_fails_when_user_data_subscription_cannot_be_created() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let (price_sender, price_receiver) = mpsc::channel(8);
        drop(price_sender);
        let market_data = Arc::new(FakeMarketData::without_user_receiver(price_receiver));
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = GridManager::new(clock);
        manager
            .add_grid(
                GridId::new("BTCUSDT"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let write_service = Arc::new(GridWriteService::new(
            manager,
            persistence.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            Arc::new(GridQueryService::new(
                persistence.clone() as Arc<dyn GridReadRepositoryPort>
            )),
            Arc::new(GridProjector::new()),
        );

        let runtime = ServerRuntime::new(
            state,
            exchange as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let error = runtime.start().await.err().unwrap();
        assert!(error.to_string().contains("missing test user receiver"));
    }

    struct RuntimeFixture {
        runtime: ServerRuntime,
        state: ServerState,
        exchange: Arc<FakeExchange>,
        persistence: Arc<MemoryPersistence>,
        price_sender: mpsc::Sender<PriceTick>,
        user_sender: mpsc::Sender<UserDataEvent>,
    }

    async fn runtime_fixture(
        restored_snapshot: Option<GridSnapshot>,
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        budget: CapacityBudget,
    ) -> RuntimeFixture {
        let exchange = Arc::new(FakeExchange::new(position, open_orders));
        let persistence = Arc::new(MemoryPersistence::default());
        let (price_sender, price_receiver) = mpsc::channel(8);
        let (user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::new(price_receiver, user_receiver));
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = GridManager::new(clock);
        manager
            .add_grid(
                GridId::new("BTCUSDT"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                test_config(),
                budget,
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        if let Some(snapshot) = restored_snapshot.clone() {
            manager.restore_grid_state(&snapshot).unwrap();
            persistence
                .save_transition("BTCUSDT", &snapshot, &[], &[])
                .await
                .unwrap();
        }

        let (events, _) = broadcast::channel(16);
        let write_service = Arc::new(GridWriteService::new(
            manager,
            persistence.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            Arc::new(GridQueryService::new(
                persistence.clone() as Arc<dyn GridReadRepositoryPort>
            )),
            Arc::new(GridProjector::new()),
        );

        RuntimeFixture {
            runtime: ServerRuntime::new(
                state.clone(),
                exchange.clone() as Arc<dyn ExchangePort>,
                market_data as Arc<dyn MarketDataPort>,
            ),
            state,
            exchange,
            persistence,
            price_sender,
            user_sender,
        }
    }

    async fn test_state<R>(
        exchange: Arc<dyn ExchangePort>,
        persistence: Arc<R>,
        restored_snapshot: Option<GridSnapshot>,
        budget: CapacityBudget,
    ) -> ServerState
    where
        R: StateRepositoryPort + GridReadRepositoryPort + 'static,
    {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = GridManager::new(clock);
        let instrument = btc_instrument();
        manager
            .add_grid(
                GridId::new("BTCUSDT"),
                instrument.clone(),
                test_config(),
                budget,
                exchange.get_exchange_info(&instrument).await.unwrap().rules,
            )
            .unwrap();
        if let Some(snapshot) = restored_snapshot {
            manager.restore_grid_state(&snapshot).unwrap();
        }

        let (events, _) = broadcast::channel(16);
        let state_repository: Arc<dyn StateRepositoryPort> = persistence.clone();
        let read_repository: Arc<dyn GridReadRepositoryPort> = persistence;
        let write_service = Arc::new(GridWriteService::new(
            manager,
            state_repository,
            events.clone(),
        ));
        build_server_state(
            write_service,
            Arc::new(GridQueryService::new(read_repository)),
            Arc::new(GridProjector::new()),
        )
    }

    async fn current_instance(state: &ServerState) -> GridRuntime {
        let manager_handle = state.write_service.manager();
        let manager = manager_handle.read().await;
        manager.get_grid("BTCUSDT").unwrap().clone()
    }

    async fn shutdown(handles: RuntimeHandles) {
        handles.market_task.abort();
        handles.user_task.abort();
        handles.effect_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
        let _ = handles.effect_task.await;
    }

    async fn wait_until<F>(condition: F)
    where
        F: Fn() -> bool,
    {
        timeout(Duration::from_secs(1), async {
            loop {
                if condition() {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_until_instance<F>(state: &ServerState, predicate: F)
    where
        F: Fn(&GridRuntime) -> bool,
    {
        timeout(Duration::from_secs(1), async {
            loop {
                if predicate(&current_instance(state).await) {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_until_async<F, Fut>(condition: F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = bool>,
    {
        timeout(Duration::from_secs(1), async {
            loop {
                if condition().await {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    fn ready_pending_effects(effects: &[PersistedGridEffect]) -> Vec<PersistedGridEffect> {
        effects
            .iter()
            .filter(|effect| {
                effect.status == EffectStatus::Pending
                    && !effects.iter().any(|prior| {
                        prior.grid_id == effect.grid_id
                            && prior.batch_id == effect.batch_id
                            && prior.sequence < effect.sequence
                            && prior.status != EffectStatus::Succeeded
                    })
            })
            .cloned()
            .collect()
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

    fn test_server_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()
    }

    fn btc_instrument() -> Instrument {
        Instrument::new(Venue::Binance, "BTCUSDT")
    }

    fn btc_position(qty: f64, unrealized_pnl: f64) -> Position {
        Position {
            instrument: btc_instrument(),
            qty,
            avg_price: 100.0,
            unrealized_pnl,
        }
    }

    fn btc_tick(reference_price: f64) -> PriceTick {
        PriceTick {
            instrument: btc_instrument(),
            reference_price,
            mark_price: reference_price,
            timestamp: Utc::now(),
        }
    }

    fn btc_exchange_order(
        order_id: &str,
        client_order_id: &str,
        side: Side,
        price: f64,
        qty: f64,
        realized_pnl: f64,
        status: OrderStatus,
    ) -> ExchangeOrder {
        ExchangeOrder {
            instrument: btc_instrument(),
            order_id: order_id.into(),
            client_order_id: client_order_id.into(),
            side,
            price,
            qty,
            realized_pnl,
            status,
        }
    }

    fn position_event_at(
        event_time: chrono::DateTime<Utc>,
        qty: f64,
        unrealized_pnl: f64,
    ) -> UserDataEvent {
        UserDataEvent {
            event_time,
            payload: UserDataPayload::PositionUpdate(btc_position(qty, unrealized_pnl)),
        }
    }

    fn order_event_at(event_time: chrono::DateTime<Utc>, order: ExchangeOrder) -> UserDataEvent {
        UserDataEvent {
            event_time,
            payload: UserDataPayload::OrderUpdate(order),
        }
    }

    fn test_snapshot() -> GridSnapshot {
        GridSnapshot {
            grid_id: GridId::new("BTCUSDT"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            config: test_config(),
            status: GridStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(6.0)),
            pending_order: Some(PendingOrder {
                order_id: Some("snapshot-1".into()),
                client_order_id: "snapshot-1".into(),
                side: Side::Buy,
                price: 94.0,
                quantity: 0.25,
                target_exposure: Exposure(6.0),
                status: OrderStatus::New,
            }),
            risk: RiskState::default(),
            observed: grid_engine::snapshot::ObservedState {
                reference_price: Some(95.0),
                out_of_band_since: Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap()),
            },
        }
    }

    struct FixedClock(chrono::DateTime<Utc>);

    impl ClockPort for FixedClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            self.0
        }
    }

    struct FakeExchange {
        exchange_info: ExchangeInfo,
        position: Mutex<Position>,
        open_orders: Mutex<Vec<ExchangeOrder>>,
        submitted_orders: Mutex<Vec<OrderRequest>>,
        cancel_all_symbols: Mutex<Vec<String>>,
        submit_error: Mutex<Option<String>>,
        cancel_all_error: Mutex<Option<String>>,
        server_time: chrono::DateTime<Utc>,
        sequence: AtomicUsize,
        submit_started: Option<Arc<Notify>>,
        release_submit: Option<Arc<Notify>>,
    }

    impl FakeExchange {
        fn new(position: Position, open_orders: Vec<ExchangeOrder>) -> Self {
            Self {
                exchange_info: ExchangeInfo {
                    instrument: btc_instrument(),
                    rules: ExchangeRules {
                        price_tick: 0.1,
                        quantity_step: 0.1,
                        min_qty: 0.0,
                        min_notional: 0.0,
                    },
                },
                position: Mutex::new(position),
                open_orders: Mutex::new(open_orders),
                submitted_orders: Mutex::new(Vec::new()),
                cancel_all_symbols: Mutex::new(Vec::new()),
                submit_error: Mutex::new(None),
                cancel_all_error: Mutex::new(None),
                server_time: test_server_time(),
                sequence: AtomicUsize::new(1),
                submit_started: None,
                release_submit: None,
            }
        }

        fn with_submit_error(
            position: Position,
            open_orders: Vec<ExchangeOrder>,
            error: &str,
        ) -> Self {
            let exchange = Self::new(position, open_orders);
            *exchange.submit_error.lock().unwrap() = Some(error.to_string());
            exchange
        }

        fn with_cancel_all_error(
            position: Position,
            open_orders: Vec<ExchangeOrder>,
            error: &str,
        ) -> Self {
            let exchange = Self::new(position, open_orders);
            *exchange.cancel_all_error.lock().unwrap() = Some(error.to_string());
            exchange
        }

        fn with_blocked_submit(
            position: Position,
            open_orders: Vec<ExchangeOrder>,
            submit_started: Arc<Notify>,
            release_submit: Arc<Notify>,
        ) -> Self {
            let mut exchange = Self::new(position, open_orders);
            exchange.submit_started = Some(submit_started);
            exchange.release_submit = Some(release_submit);
            exchange
        }
    }

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
            self.submitted_orders.lock().unwrap().push(req.clone());
            if let Some(notify) = &self.submit_started {
                notify.notify_waiters();
            }
            if let Some(notify) = &self.release_submit {
                notify.notified().await;
            }
            if let Some(error) = self.submit_error.lock().unwrap().clone() {
                return Err(anyhow!(error));
            }
            let order_id = self.sequence.fetch_add(1, Ordering::SeqCst);
            Ok(OrderReceipt {
                order_id: format!("order-{order_id}"),
                client_order_id: req.client_order_id,
                status: OrderStatus::New,
            })
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
            self.cancel_all_symbols
                .lock()
                .unwrap()
                .push(instrument.symbol.clone());
            if let Some(error) = self.cancel_all_error.lock().unwrap().clone() {
                return Err(anyhow!(error));
            }
            Ok(())
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            Ok(self.position.lock().unwrap().clone())
        }

        async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            Ok(self.open_orders.lock().unwrap().clone())
        }

        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(self.exchange_info.clone())
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(self.server_time)
        }
    }

    #[derive(Default)]
    struct MemoryPersistence {
        snapshots: AsyncMutex<HashMap<String, GridSnapshot>>,
        effects: AsyncMutex<Vec<PersistedGridEffect>>,
        next_effect_batch: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryPersistence {
        async fn save_transition(
            &self,
            id: &str,
            state: &GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
            effects: &[ExecutionAction],
        ) -> Result<CommittedGridWrite> {
            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());

            let now = Utc::now();
            let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
            let mut effect_store = self.effects.lock().await;
            let mut persisted_effects = Vec::new();
            for (sequence, effect) in effects.iter().enumerate() {
                if matches!(effect, ExecutionAction::NoOp) {
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

            Ok(CommittedGridWrite {
                grid_id: GridId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects))
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

    impl MemoryPersistence {
        async fn all_effects(&self) -> Vec<PersistedGridEffect> {
            self.effects.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl GridReadRepositoryPort for MemoryPersistence {
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

    #[derive(Default)]
    struct FailOnReceiptPersistence {
        snapshots: AsyncMutex<HashMap<String, GridSnapshot>>,
        effects: AsyncMutex<Vec<PersistedGridEffect>>,
        next_effect_batch: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailOnReceiptPersistence {
        async fn save_transition(
            &self,
            id: &str,
            state: &GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
            effects: &[ExecutionAction],
        ) -> Result<CommittedGridWrite> {
            if state
                .pending_order
                .as_ref()
                .and_then(|pending| pending.order_id.as_ref())
                .is_some()
            {
                return Err(anyhow!("injected receipt persistence failure"));
            }

            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());

            let now = Utc::now();
            let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
            let mut effect_store = self.effects.lock().await;
            let mut persisted_effects = Vec::new();
            for (sequence, effect) in effects.iter().enumerate() {
                if matches!(effect, ExecutionAction::NoOp) {
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

            Ok(CommittedGridWrite {
                grid_id: GridId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects))
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

    impl FailOnReceiptPersistence {
        async fn all_effects(&self) -> Vec<PersistedGridEffect> {
            self.effects.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl GridReadRepositoryPort for FailOnReceiptPersistence {
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

    struct FailOnSavePersistence {
        snapshots: AsyncMutex<HashMap<String, GridSnapshot>>,
        effects: AsyncMutex<Vec<PersistedGridEffect>>,
        next_effect_batch: AtomicUsize,
        save_count: AtomicUsize,
        fail_on: usize,
    }

    impl FailOnSavePersistence {
        fn new(fail_on: usize) -> Self {
            Self {
                snapshots: AsyncMutex::new(HashMap::new()),
                effects: AsyncMutex::new(Vec::new()),
                next_effect_batch: AtomicUsize::new(0),
                save_count: AtomicUsize::new(0),
                fail_on,
            }
        }

        async fn all_effects(&self) -> Vec<PersistedGridEffect> {
            self.effects.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailOnSavePersistence {
        async fn save_transition(
            &self,
            id: &str,
            state: &GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
            effects: &[ExecutionAction],
        ) -> Result<CommittedGridWrite> {
            let save_number = self.save_count.fetch_add(1, Ordering::SeqCst) + 1;
            if save_number == self.fail_on {
                return Err(anyhow!("injected save failure"));
            }

            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());

            let now = Utc::now();
            let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
            let mut effect_store = self.effects.lock().await;
            let mut persisted_effects = Vec::new();
            for (sequence, effect) in effects.iter().enumerate() {
                if matches!(effect, ExecutionAction::NoOp) {
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

            Ok(CommittedGridWrite {
                grid_id: GridId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects))
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
    impl GridReadRepositoryPort for FailOnSavePersistence {
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

    struct FakeMarketData {
        price_receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
        user_receiver: Mutex<Option<mpsc::Receiver<UserDataEvent>>>,
    }

    impl FakeMarketData {
        fn new(
            price_receiver: mpsc::Receiver<PriceTick>,
            user_receiver: mpsc::Receiver<UserDataEvent>,
        ) -> Self {
            let mut price_receivers = HashMap::new();
            price_receivers.insert("BTCUSDT".to_string(), price_receiver);
            Self {
                price_receivers: Mutex::new(price_receivers),
                user_receiver: Mutex::new(Some(user_receiver)),
            }
        }

        fn without_user_receiver(price_receiver: mpsc::Receiver<PriceTick>) -> Self {
            let mut price_receivers = HashMap::new();
            price_receivers.insert("BTCUSDT".to_string(), price_receiver);
            Self {
                price_receivers: Mutex::new(price_receivers),
                user_receiver: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl MarketDataPort for FakeMarketData {
        async fn subscribe_prices(
            &self,
            instrument: &Instrument,
        ) -> Result<mpsc::Receiver<PriceTick>> {
            self.price_receivers
                .lock()
                .unwrap()
                .remove(&instrument.symbol)
                .ok_or_else(|| anyhow!("missing test price receiver for {}", instrument.symbol))
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let receiver = self
                .user_receiver
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow!("missing test user receiver"))?;

            Ok(receiver)
        }
    }
}
