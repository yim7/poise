use std::sync::Arc;

use anyhow::{Result, anyhow};
use grid_core::types::Exposure;
use grid_engine::execution_plan::ExecutionAction;
use grid_engine::instance::PendingOrder;
use grid_engine::ports::{ExchangePort, MarketDataPort, OrderReceipt, UserDataEvent};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use crate::assembly::{AppState, MutateAndPersistError, mutate_instance_and_persist};
use crate::websocket::WsEvent;

#[derive(Clone)]
pub struct Runtime {
    state: AppState,
    exchange: Arc<dyn ExchangePort>,
    market_data: Arc<dyn MarketDataPort>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub struct RuntimeHandles {
    #[cfg_attr(not(test), allow(dead_code))]
    pub market_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub user_task: JoinHandle<()>,
}

impl Runtime {
    pub fn new(
        state: AppState,
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
        self.startup_sync().await?;
        self.drain_buffered_user_events(&mut user_receiver).await?;

        let market_task = self.spawn_market_task();
        let user_task = self.spawn_user_task(user_receiver);

        Ok(RuntimeHandles {
            market_task,
            user_task,
        })
    }

    async fn startup_sync(&self) -> Result<()> {
        for binding in instance_bindings(&self.state).await {
            let position = self.exchange.get_position(&binding.symbol).await?;
            let open_orders = self.exchange.get_open_orders(&binding.symbol).await?;
            mutate_instance_and_persist(&self.state, &binding.id, |manager| {
                manager.clear_pending_order(&binding.symbol)?;
                manager.apply_position_update(&position)?;
                for order in &open_orders {
                    manager.apply_order_update(order)?;
                }
                Ok(())
            })
            .await
            .map_err(mutate_error)?;
        }

        Ok(())
    }

    async fn drain_buffered_user_events(
        &self,
        receiver: &mut mpsc::Receiver<UserDataEvent>,
    ) -> Result<()> {
        while let Ok(event) = receiver.try_recv() {
            apply_user_data_event(&self.state, event)
                .await
                .map_err(mutate_error)?;
        }

        Ok(())
    }

    fn spawn_market_task(&self) -> JoinHandle<()> {
        let state = self.state.clone();
        let exchange = Arc::clone(&self.exchange);
        let market_data = Arc::clone(&self.market_data);

        tokio::spawn(async move {
            let bindings = instance_bindings(&state).await;
            let mut workers = JoinSet::new();

            for binding in bindings {
                match market_data.subscribe_prices(&binding.symbol).await {
                    Ok(mut receiver) => {
                        let state = state.clone();
                        let exchange = Arc::clone(&exchange);
                        workers.spawn(async move {
                            while let Some(tick) = receiver.recv().await {
                                match mutate_instance_and_persist(&state, &binding.id, |manager| {
                                    manager.on_price_tick(&tick)
                                })
                                .await
                                {
                                    Ok(outcome) => {
                                        for action in outcome.plan.actions {
                                            if let Err(error) = execute_action(
                                                &state,
                                                exchange.as_ref(),
                                                &binding.id,
                                                &binding.symbol,
                                                action,
                                            )
                                            .await
                                            {
                                                tracing::warn!(
                                                    "failed to execute plan action for {}: {error}",
                                                    binding.symbol
                                                );
                                                break;
                                            }
                                        }

                                        for event in outcome.events {
                                            let _ = state.events.send(WsEvent {
                                                instance_id: binding.id.clone(),
                                                event,
                                            });
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            "failed to apply market data update for {}: {}",
                                            binding.symbol,
                                            error.message()
                                        );
                                    }
                                }
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(
                            "failed to subscribe market data for {}: {error}",
                            binding.symbol
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

    fn spawn_user_task(&self, mut receiver: mpsc::Receiver<UserDataEvent>) -> JoinHandle<()> {
        let state = self.state.clone();

        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let symbol = event_symbol(&event);
                if let Err(error) = apply_user_data_event(&state, event).await {
                    tracing::warn!(
                        "failed to apply user data update for {symbol}: {}",
                        error.message()
                    );
                }
            }
        })
    }
}

#[derive(Clone)]
struct InstanceBinding {
    id: String,
    symbol: String,
}

async fn execute_action(
    state: &AppState,
    exchange: &dyn ExchangePort,
    instance_id: &str,
    symbol: &str,
    action: ExecutionAction,
) -> Result<()> {
    match action {
        ExecutionAction::CancelAll => {
            exchange.cancel_all(symbol).await?;
            mutate_instance_and_persist(state, instance_id, |manager| {
                manager.clear_pending_order(symbol)
            })
            .await
            .map_err(mutate_error)?;
        }
        ExecutionAction::CancelOrder { order_id } => {
            exchange.cancel_order(symbol, &order_id).await?;
            mutate_instance_and_persist(state, instance_id, |manager| {
                manager.clear_pending_order(symbol)
            })
            .await
            .map_err(mutate_error)?;
        }
        ExecutionAction::SubmitOrder {
            request,
            target_exposure,
        } => {
            let receipt = exchange.submit_order(request.clone()).await?;
            record_submitted_order(state, instance_id, &request, &receipt, target_exposure).await?;
        }
        ExecutionAction::NoOp => {}
    }

    Ok(())
}

fn event_symbol(event: &UserDataEvent) -> String {
    match event {
        UserDataEvent::PositionUpdate(position) => position.symbol.clone(),
        UserDataEvent::OrderUpdate(order) => order.symbol.clone(),
    }
}

async fn apply_user_data_event(
    state: &AppState,
    event: UserDataEvent,
) -> std::result::Result<(), MutateAndPersistError> {
    let symbol = event_symbol(&event);

    let Some(instance_id) = instance_id_for_symbol(state, &symbol).await else {
        tracing::warn!("received user data for unknown symbol {symbol}");
        return Ok(());
    };

    match event {
        UserDataEvent::PositionUpdate(position) => {
            mutate_instance_and_persist(state, &instance_id, |manager| {
                manager.apply_position_update(&position)
            })
            .await?;
        }
        UserDataEvent::OrderUpdate(order) => {
            mutate_instance_and_persist(state, &instance_id, |manager| {
                manager.apply_order_update(&order)
            })
            .await?;
        }
    }

    Ok(())
}

async fn record_submitted_order(
    state: &AppState,
    instance_id: &str,
    request: &grid_engine::ports::OrderRequest,
    receipt: &OrderReceipt,
    target_exposure: Exposure,
) -> Result<()> {
    mutate_instance_and_persist(state, instance_id, |manager| {
        manager.record_submitted_order(
            instance_id,
            PendingOrder {
                symbol: request.symbol.clone(),
                order_id: Some(receipt.order_id.clone()),
                client_order_id: receipt.client_order_id.clone(),
                side: request.side,
                price: request.price,
                quantity: request.quantity,
                target_exposure: target_exposure.clone(),
                status: receipt.status.clone(),
            },
        )
    })
    .await
    .map_err(mutate_error)?;

    Ok(())
}

async fn instance_bindings(state: &AppState) -> Vec<InstanceBinding> {
    let manager = state.manager.read().await;
    manager
        .list_instances()
        .into_iter()
        .map(|instance| InstanceBinding {
            id: instance.id.clone(),
            symbol: instance.symbol.clone(),
        })
        .collect()
}

async fn instance_id_for_symbol(state: &AppState, symbol: &str) -> Option<String> {
    let manager = state.manager.read().await;
    manager
        .list_instances()
        .into_iter()
        .find(|instance| instance.symbol == symbol)
        .map(|instance| instance.id.clone())
}

fn mutate_error(error: MutateAndPersistError) -> anyhow::Error {
    anyhow!(error.message())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{ExchangeRules, Exposure, Side};
    use grid_engine::execution_plan::ExecutionAction;
    use grid_engine::instance::{InstanceStatus, PendingOrder, RiskState, StrategyInstance};
    use grid_engine::manager::InstanceManager;
    use grid_engine::ports::{
        ClockPort, ExchangeInfo, ExchangePort, InstanceSnapshot, MarketDataPort, OpenOrder,
        OrderReceipt, OrderRequest, PersistencePort, Position, PriceTick, UserDataEvent,
    };
    use tokio::sync::{Mutex as AsyncMutex, RwLock, broadcast, mpsc};
    use tokio::time::{sleep, timeout};

    use crate::assembly::{AppState, mutate_instance_and_persist};

    use super::{Runtime, RuntimeHandles, record_submitted_order};

    #[tokio::test]
    async fn market_tick_submits_order_and_records_pending_order() {
        let fixture = runtime_fixture(
            None,
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            vec![],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture
            .price_sender
            .send(PriceTick {
                symbol: "BTCUSDT".into(),
                last_price: 95.0,
                mark_price: 95.0,
                timestamp: Utc::now(),
            })
            .await
            .unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_instance(&fixture.state, |instance| instance.pending_order.is_some()).await;

        let instance = current_instance(&fixture.state).await;
        let pending = instance.pending_order.unwrap();
        assert_eq!(pending.order_id.as_deref(), Some("order-1"));
        assert_eq!(pending.target_exposure, Exposure(4.0));

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn submitted_order_keeps_action_target_when_instance_changes_before_recording() {
        let fixture = runtime_fixture(
            None,
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            vec![],
            test_budget(),
        )
        .await;

        let tick = PriceTick {
            symbol: "BTCUSDT".into(),
            last_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };
        let outcome = mutate_instance_and_persist(&fixture.state, "BTCUSDT", |manager| {
            manager.on_price_tick(&tick)
        })
        .await
        .unwrap();
        let (request, target_exposure) = match outcome.plan.actions.as_slice() {
            [
                ExecutionAction::SubmitOrder {
                    request,
                    target_exposure,
                },
            ] => (request.clone(), target_exposure.clone()),
            other => panic!("unexpected actions: {other:?}"),
        };

        mutate_instance_and_persist(&fixture.state, "BTCUSDT", |manager| {
            manager.pause_instance("BTCUSDT")
        })
        .await
        .unwrap();

        let receipt = fixture
            .exchange
            .submit_order(request.clone())
            .await
            .unwrap();
        record_submitted_order(
            &fixture.state,
            "BTCUSDT",
            &request,
            &receipt,
            target_exposure.clone(),
        )
        .await
        .unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.target_exposure, None);
        assert_eq!(
            instance
                .pending_order
                .map(|pending| pending.target_exposure),
            Some(target_exposure)
        );
    }

    #[tokio::test]
    async fn position_update_reconciles_actual_exposure_without_overwriting_target() {
        let fixture = runtime_fixture(
            None,
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            vec![],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture
            .price_sender
            .send(PriceTick {
                symbol: "BTCUSDT".into(),
                last_price: 95.0,
                mark_price: 95.0,
                timestamp: Utc::now(),
            })
            .await
            .unwrap();
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
            .send(UserDataEvent::PositionUpdate(Position {
                symbol: "BTCUSDT".into(),
                qty: 7.5,
                avg_price: 100.0,
                unrealized_pnl: 11.0,
            }))
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
    async fn order_update_clears_pending_order_on_terminal_status() {
        let fixture = runtime_fixture(
            None,
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            vec![],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture
            .price_sender
            .send(PriceTick {
                symbol: "BTCUSDT".into(),
                last_price: 95.0,
                mark_price: 95.0,
                timestamp: Utc::now(),
            })
            .await
            .unwrap();
        wait_until_instance(&fixture.state, |instance| instance.pending_order.is_some()).await;

        let pending = current_instance(&fixture.state)
            .await
            .pending_order
            .unwrap();

        fixture
            .user_sender
            .send(UserDataEvent::OrderUpdate(OpenOrder {
                symbol: "BTCUSDT".into(),
                order_id: pending.order_id.clone().unwrap(),
                client_order_id: pending.client_order_id.clone(),
                side: Side::Buy,
                price: pending.price,
                qty: pending.quantity,
                realized_pnl: 0.0,
                status: "FILLED".into(),
            }))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| instance.pending_order.is_none()).await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_uses_live_position_and_open_orders_before_first_tick() {
        let snapshot = test_snapshot();
        let live_order = OpenOrder {
            symbol: "BTCUSDT".into(),
            order_id: "live-1".into(),
            client_order_id: "live-1".into(),
            side: Side::Buy,
            price: 94.5,
            qty: 0.25,
            realized_pnl: 0.0,
            status: "NEW".into(),
        };
        let fixture = runtime_fixture(
            Some(snapshot),
            Position {
                symbol: "BTCUSDT".into(),
                qty: 7.5,
                avg_price: 100.0,
                unrealized_pnl: 3.0,
            },
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

        fixture
            .price_sender
            .send(PriceTick {
                symbol: "BTCUSDT".into(),
                last_price: 92.5,
                mark_price: 92.5,
                timestamp: Utc::now(),
            })
            .await
            .unwrap();
        sleep(Duration::from_millis(100)).await;

        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_clears_stale_pending_order_when_exchange_has_no_open_orders() {
        let fixture = runtime_fixture(
            Some(test_snapshot()),
            Position {
                symbol: "BTCUSDT".into(),
                qty: 7.5,
                avg_price: 100.0,
                unrealized_pnl: 3.0,
            },
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
    async fn startup_sync_replays_buffered_user_event_before_first_tick() {
        let (price_sender, price_receiver) = mpsc::channel(8);
        drop(price_sender);
        let market_data = Arc::new(BufferedUserDataMarket::new(price_receiver));
        let exchange = Arc::new(StartupInjectingExchange::new(
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            market_data.user_sender_handle(),
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = InstanceManager::new(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone() as Arc<dyn PersistencePort>,
            clock,
        );
        manager
            .add_instance(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let state = AppState {
            manager: Arc::new(RwLock::new(manager)),
            persistence,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
            events,
        };
        let runtime = Runtime::new(
            state.clone(),
            exchange as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let handles = runtime.start().await.unwrap();

        wait_until_instance(&state, |instance| {
            (instance.current_exposure.0 - 2.0).abs() < f64::EPSILON
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn filled_order_updates_realized_pnl_and_trips_daily_loss_cap() {
        let fixture = runtime_fixture(
            None,
            Position {
                symbol: "BTCUSDT".into(),
                qty: 7.5,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
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
            .send(UserDataEvent::OrderUpdate(OpenOrder {
                symbol: "BTCUSDT".into(),
                order_id: "fill-1".into(),
                client_order_id: "fill-1".into(),
                side: Side::Sell,
                price: 95.0,
                qty: 7.5,
                realized_pnl: -20.0,
                status: "FILLED".into(),
            }))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| {
            (instance.risk_state.realized_pnl_today + 20.0).abs() < f64::EPSILON
        })
        .await;

        fixture
            .price_sender
            .send(PriceTick {
                symbol: "BTCUSDT".into(),
                last_price: 95.0,
                mark_price: 95.0,
                timestamp: Utc::now(),
            })
            .await
            .unwrap();

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
        let exchange = Arc::new(FakeExchange::new(
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            vec![],
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let (price_sender, price_receiver) = mpsc::channel(8);
        drop(price_sender);
        let market_data = Arc::new(FakeMarketData::without_user_receiver(price_receiver));
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = InstanceManager::new(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone() as Arc<dyn PersistencePort>,
            clock,
        );
        manager
            .add_instance(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let state = AppState {
            manager: Arc::new(RwLock::new(manager)),
            persistence,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
            events,
        };

        let runtime = Runtime::new(
            state,
            exchange as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let error = runtime.start().await.err().unwrap();
        assert!(error.to_string().contains("missing test user receiver"));
    }

    struct RuntimeFixture {
        runtime: Runtime,
        state: AppState,
        exchange: Arc<FakeExchange>,
        price_sender: mpsc::Sender<PriceTick>,
        user_sender: mpsc::Sender<UserDataEvent>,
    }

    async fn runtime_fixture(
        restored_snapshot: Option<InstanceSnapshot>,
        position: Position,
        open_orders: Vec<OpenOrder>,
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

        let mut manager = InstanceManager::new(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone() as Arc<dyn PersistencePort>,
            clock,
        );
        manager
            .add_instance(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                test_config(),
                budget,
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        if let Some(snapshot) = restored_snapshot.clone() {
            manager.restore_instance_state(&snapshot).unwrap();
            persistence
                .save_instance_state("BTCUSDT", &snapshot)
                .await
                .unwrap();
        }

        let (events, _) = broadcast::channel(16);
        let state = AppState {
            manager: Arc::new(RwLock::new(manager)),
            persistence,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
            events,
        };

        RuntimeFixture {
            runtime: Runtime::new(
                state.clone(),
                exchange.clone() as Arc<dyn ExchangePort>,
                market_data as Arc<dyn MarketDataPort>,
            ),
            state,
            exchange,
            price_sender,
            user_sender,
        }
    }

    async fn current_instance(state: &AppState) -> grid_engine::instance::StrategyInstance {
        let manager = state.manager.read().await;
        manager.get_instance("BTCUSDT").unwrap().clone()
    }

    async fn shutdown(handles: RuntimeHandles) {
        handles.market_task.abort();
        handles.user_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
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

    async fn wait_until_instance<F>(state: &AppState, predicate: F)
    where
        F: Fn(&StrategyInstance) -> bool,
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

    fn test_config() -> GridConfig {
        GridConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_capacity: 8.0,
            short_capacity: 8.0,
            capacity_notional: 375.0,
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

    fn test_snapshot() -> InstanceSnapshot {
        InstanceSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            config: test_config(),
            status: InstanceStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(6.0)),
            pending_order: Some(PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("snapshot-1".into()),
                client_order_id: "snapshot-1".into(),
                side: Side::Buy,
                price: 94.0,
                quantity: 0.25,
                target_exposure: Exposure(6.0),
                status: "NEW".into(),
            }),
            risk_state: RiskState::default(),
            last_price: Some(95.0),
            out_of_band_since: Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap()),
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
        open_orders: Mutex<Vec<OpenOrder>>,
        submitted_orders: Mutex<Vec<OrderRequest>>,
        cancel_all_symbols: Mutex<Vec<String>>,
        sequence: AtomicUsize,
    }

    impl FakeExchange {
        fn new(position: Position, open_orders: Vec<OpenOrder>) -> Self {
            Self {
                exchange_info: ExchangeInfo {
                    symbol: "BTCUSDT".into(),
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
                sequence: AtomicUsize::new(1),
            }
        }
    }

    struct StartupInjectingExchange {
        exchange_info: ExchangeInfo,
        position: Mutex<Position>,
        user_sender: Arc<Mutex<Option<mpsc::Sender<UserDataEvent>>>>,
        injected_update: AtomicUsize,
    }

    impl StartupInjectingExchange {
        fn new(
            position: Position,
            user_sender: Arc<Mutex<Option<mpsc::Sender<UserDataEvent>>>>,
        ) -> Self {
            Self {
                exchange_info: ExchangeInfo {
                    symbol: "BTCUSDT".into(),
                    rules: ExchangeRules {
                        price_tick: 0.1,
                        quantity_step: 0.1,
                        min_qty: 0.0,
                        min_notional: 0.0,
                    },
                },
                position: Mutex::new(position),
                user_sender,
                injected_update: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
            self.submitted_orders.lock().unwrap().push(req.clone());
            let order_id = self.sequence.fetch_add(1, Ordering::SeqCst);
            Ok(OrderReceipt {
                order_id: format!("order-{order_id}"),
                client_order_id: req.client_order_id,
                status: "NEW".into(),
            })
        }

        async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self, symbol: &str) -> Result<()> {
            self.cancel_all_symbols
                .lock()
                .unwrap()
                .push(symbol.to_string());
            Ok(())
        }

        async fn get_position(&self, _symbol: &str) -> Result<Position> {
            Ok(self.position.lock().unwrap().clone())
        }

        async fn get_open_orders(&self, _symbol: &str) -> Result<Vec<OpenOrder>> {
            Ok(self.open_orders.lock().unwrap().clone())
        }

        async fn get_exchange_info(&self, _symbol: &str) -> Result<ExchangeInfo> {
            Ok(self.exchange_info.clone())
        }
    }

    #[async_trait::async_trait]
    impl ExchangePort for StartupInjectingExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            unreachable!()
        }

        async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
            unreachable!()
        }

        async fn cancel_all(&self, _symbol: &str) -> Result<()> {
            unreachable!()
        }

        async fn get_position(&self, _symbol: &str) -> Result<Position> {
            Ok(self.position.lock().unwrap().clone())
        }

        async fn get_open_orders(&self, _symbol: &str) -> Result<Vec<OpenOrder>> {
            if self.injected_update.fetch_add(1, Ordering::SeqCst) == 0 {
                let sender = self.user_sender.lock().unwrap().clone();
                if let Some(sender) = sender {
                    sender
                        .send(UserDataEvent::PositionUpdate(Position {
                            symbol: "BTCUSDT".into(),
                            qty: 7.5,
                            avg_price: 100.0,
                            unrealized_pnl: 5.0,
                        }))
                        .await
                        .unwrap();
                }
            }

            Ok(Vec::new())
        }

        async fn get_exchange_info(&self, _symbol: &str) -> Result<ExchangeInfo> {
            Ok(self.exchange_info.clone())
        }
    }

    #[derive(Default)]
    struct MemoryPersistence {
        snapshots: AsyncMutex<HashMap<String, InstanceSnapshot>>,
    }

    #[async_trait::async_trait]
    impl PersistencePort for MemoryPersistence {
        async fn save_instance_state(&self, id: &str, state: &InstanceSnapshot) -> Result<()> {
            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());
            Ok(())
        }

        async fn load_instance_state(&self, id: &str) -> Result<Option<InstanceSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
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

    struct BufferedUserDataMarket {
        price_receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
        user_sender: Arc<Mutex<Option<mpsc::Sender<UserDataEvent>>>>,
    }

    impl BufferedUserDataMarket {
        fn new(price_receiver: mpsc::Receiver<PriceTick>) -> Self {
            let mut price_receivers = HashMap::new();
            price_receivers.insert("BTCUSDT".to_string(), price_receiver);
            Self {
                price_receivers: Mutex::new(price_receivers),
                user_sender: Arc::new(Mutex::new(None)),
            }
        }

        fn user_sender_handle(&self) -> Arc<Mutex<Option<mpsc::Sender<UserDataEvent>>>> {
            Arc::clone(&self.user_sender)
        }
    }

    #[async_trait::async_trait]
    impl MarketDataPort for FakeMarketData {
        async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>> {
            self.price_receivers
                .lock()
                .unwrap()
                .remove(symbol)
                .ok_or_else(|| anyhow!("missing test price receiver for {symbol}"))
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            self.user_receiver
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow!("missing test user receiver"))
        }
    }

    #[async_trait::async_trait]
    impl MarketDataPort for BufferedUserDataMarket {
        async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>> {
            self.price_receivers
                .lock()
                .unwrap()
                .remove(symbol)
                .ok_or_else(|| anyhow!("missing test price receiver for {symbol}"))
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let (sender, receiver) = mpsc::channel(8);
            *self.user_sender.lock().unwrap() = Some(sender);
            Ok(receiver)
        }
    }
}
