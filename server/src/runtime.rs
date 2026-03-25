use std::sync::Arc;

use anyhow::{Result, anyhow};
use grid_core::types::Exposure;
use grid_engine::execution_plan::ExecutionAction;
use grid_engine::instance::PendingOrder;
use grid_engine::manager::TickOutcome;
use grid_engine::ports::{
    ExchangePort, MarketDataPort, OrderReceipt, OrderRequest, OrderStatus, PriceTick,
    UserDataEvent, UserDataPayload,
};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use crate::application::GridMutationError;
use crate::assembly::ServerState;
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
        let user_task = self.spawn_user_task(user_receiver, startup_cutoff);
        let market_task = self.spawn_market_task();

        Ok(RuntimeHandles {
            market_task,
            user_task,
        })
    }

    async fn startup_sync(&self) -> Result<()> {
        for binding in self.state.service.grid_bindings().await {
            let position = self.exchange.get_position(&binding.symbol).await?;
            let open_orders = self.exchange.get_open_orders(&binding.symbol).await?;
            self.state
                .service
                .mutate_grid(&binding.id, |manager| {
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
                let symbol = event_symbol(&event);
                let should_reconcile = should_reconcile_after_user_data(&event);
                apply_user_data_event(&self.state, event)
                    .await
                    .map_err(mutate_error)?;
                if should_reconcile {
                    reconcile_symbol_at_last_price(&self.state, self.exchange.as_ref(), &symbol)
                        .await?;
                }
            }
        }

        Ok(())
    }

    fn spawn_market_task(&self) -> JoinHandle<()> {
        let state = self.state.clone();
        let exchange = Arc::clone(&self.exchange);
        let market_data = Arc::clone(&self.market_data);

        tokio::spawn(async move {
            let bindings = state.service.grid_bindings().await;
            let mut workers = JoinSet::new();

            for binding in bindings {
                match market_data.subscribe_prices(&binding.symbol).await {
                    Ok(mut receiver) => {
                        let state = state.clone();
                        let exchange = Arc::clone(&exchange);
                        workers.spawn(async move {
                            while let Some(tick) = receiver.recv().await {
                                match state
                                    .service
                                    .mutate_grid(&binding.id, |manager| {
                                        manager.on_price_tick(&tick)
                                    })
                                    .await
                                {
                                    Ok(outcome) => {
                                        if let Err(error) = handle_tick_outcome(
                                            &state,
                                            exchange.as_ref(),
                                            &binding.id,
                                            &binding.symbol,
                                            outcome,
                                        )
                                        .await
                                        {
                                            tracing::warn!(
                                                "failed to execute plan action for {}: {error}",
                                                binding.symbol
                                            );
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

    fn spawn_user_task(
        &self,
        mut receiver: mpsc::Receiver<UserDataEvent>,
        startup_cutoff: chrono::DateTime<chrono::Utc>,
    ) -> JoinHandle<()> {
        let state = self.state.clone();
        let exchange = Arc::clone(&self.exchange);

        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                if event.event_time <= startup_cutoff {
                    continue;
                }

                let symbol = event_symbol(&event);
                let should_reconcile = should_reconcile_after_user_data(&event);
                if let Err(error) = apply_user_data_event(&state, event).await {
                    tracing::warn!(
                        "failed to apply user data update for {symbol}: {}",
                        error.message()
                    );
                    continue;
                }

                if should_reconcile
                    && let Err(error) =
                        reconcile_symbol_at_last_price(&state, exchange.as_ref(), &symbol).await
                {
                    tracing::warn!(
                        "failed to reconcile after user data update for {symbol}: {error}"
                    );
                }
            }
        })
    }
}

async fn execute_action(
    state: &ServerState,
    exchange: &dyn ExchangePort,
    instance_id: &str,
    symbol: &str,
    action: ExecutionAction,
) -> Result<()> {
    match action {
        ExecutionAction::CancelAll => {
            mark_pending_order_canceling(state, instance_id, symbol).await?;
            exchange.cancel_all(symbol).await?;
            state
                .service
                .mutate_grid(instance_id, |manager| manager.clear_pending_order(symbol))
                .await
                .map_err(mutate_error)?;
        }
        ExecutionAction::CancelOrder { order_id } => {
            mark_pending_order_canceling(state, instance_id, symbol).await?;
            exchange.cancel_order(symbol, &order_id).await?;
            state
                .service
                .mutate_grid(instance_id, |manager| manager.clear_pending_order(symbol))
                .await
                .map_err(mutate_error)?;
        }
        ExecutionAction::SubmitOrder {
            request,
            target_exposure,
        } => {
            record_submission_intent(state, instance_id, &request, target_exposure.clone()).await?;
            let receipt = exchange.submit_order(request.clone()).await?;
            record_submitted_order(state, instance_id, &request, &receipt, target_exposure).await?;
        }
        ExecutionAction::NoOp => {}
    }

    Ok(())
}

async fn handle_tick_outcome(
    state: &ServerState,
    exchange: &dyn ExchangePort,
    instance_id: &str,
    symbol: &str,
    outcome: TickOutcome,
) -> Result<()> {
    let mut action_error = None;
    for action in outcome.plan.actions {
        if let Err(error) = execute_action(state, exchange, instance_id, symbol, action).await {
            action_error = Some(error);
            break;
        }
    }

    match action_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn event_symbol(event: &UserDataEvent) -> String {
    event.symbol().to_string()
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
    event: UserDataEvent,
) -> std::result::Result<(), GridMutationError> {
    let symbol = event_symbol(&event);

    let Some(instance_id) = state.service.grid_id_for_symbol(&symbol).await else {
        tracing::warn!("received user data for unknown symbol {symbol}");
        return Ok(());
    };

    match event.payload {
        UserDataPayload::PositionUpdate(position) => {
            state
                .service
                .mutate_grid(&instance_id, |manager| {
                    manager.apply_position_update(&position)
                })
                .await?;
        }
        UserDataPayload::OrderUpdate(order) => {
            state
                .service
                .mutate_grid(&instance_id, |manager| manager.apply_order_update(&order))
                .await?;
        }
    }

    Ok(())
}

async fn reconcile_symbol_at_last_price(
    state: &ServerState,
    exchange: &dyn ExchangePort,
    symbol: &str,
) -> Result<()> {
    let Some((instance_id, reference_price)) =
        state.service.reconcile_context_for_symbol(symbol).await
    else {
        return Ok(());
    };

    let tick = PriceTick {
        symbol: symbol.to_string(),
        reference_price,
        mark_price: reference_price,
        timestamp: chrono::Utc::now(),
    };
    let outcome = state
        .service
        .mutate_grid(&instance_id, |manager| manager.on_price_tick(&tick))
        .await
        .map_err(mutate_error)?;

    handle_tick_outcome(state, exchange, &instance_id, symbol, outcome).await
}

async fn record_submitted_order(
    state: &ServerState,
    instance_id: &str,
    request: &OrderRequest,
    receipt: &OrderReceipt,
    target_exposure: Exposure,
) -> Result<()> {
    state
        .service
        .mutate_grid(instance_id, |manager| {
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
                    status: receipt.status,
                },
            )
        })
        .await
        .map_err(mutate_error)?;

    Ok(())
}

async fn record_submission_intent(
    state: &ServerState,
    instance_id: &str,
    request: &OrderRequest,
    target_exposure: Exposure,
) -> Result<()> {
    state
        .service
        .mutate_grid(instance_id, |manager| {
            manager.record_submitted_order(
                instance_id,
                PendingOrder {
                    symbol: request.symbol.clone(),
                    order_id: None,
                    client_order_id: request.client_order_id.clone(),
                    side: request.side,
                    price: request.price,
                    quantity: request.quantity,
                    target_exposure: target_exposure.clone(),
                    status: OrderStatus::Submitting,
                },
            )
        })
        .await
        .map_err(mutate_error)?;

    Ok(())
}

async fn mark_pending_order_canceling(
    state: &ServerState,
    instance_id: &str,
    symbol: &str,
) -> Result<()> {
    state
        .service
        .mutate_grid(instance_id, |manager| {
            let pending = manager
                .get_instance(instance_id)
                .and_then(|instance| instance.pending_order.clone())
                .filter(|pending| pending.symbol == symbol);

            if let Some(mut pending) = pending {
                pending.status = OrderStatus::Canceling;
                manager.record_submitted_order(instance_id, pending)?;
            }

            Ok(())
        })
        .await
        .map_err(mutate_error)?;

    Ok(())
}

fn mutate_error(error: GridMutationError) -> anyhow::Error {
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
    use grid_engine::instance::{GridStatus, PendingOrder, RiskState, StrategyInstance};
    use grid_engine::manager::InstanceManager;
    use grid_engine::ports::{
        ClockPort, ExchangeInfo, ExchangeOrder, ExchangePort, GridSnapshot, MarketDataPort,
        OrderReceipt, OrderRequest, OrderStatus, Position, PriceTick, StateRepositoryPort,
        UserDataEvent, UserDataPayload,
    };
    use grid_protocol::DomainEvent as ProtocolDomainEvent;
    use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
    use tokio::time::{sleep, timeout};

    use crate::application::GridPlatformService;
    use crate::assembly::ServerState;

    use super::{RuntimeHandles, ServerRuntime, execute_action, record_submitted_order};

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
                reference_price: 95.0,
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
    async fn submit_order_keeps_submission_intent_when_receipt_persistence_fails() {
        let exchange = Arc::new(FakeExchange::new(
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            vec![],
        ));
        let persistence = Arc::new(FailOnSavePersistence::new(2));
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone() as Arc<dyn StateRepositoryPort>,
            None,
            test_budget(),
        )
        .await;

        let error = execute_action(
            &state,
            exchange.as_ref(),
            "BTCUSDT",
            "BTCUSDT",
            ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    symbol: "BTCUSDT".into(),
                    side: Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    client_order_id: "intent-1".into(),
                },
                target_exposure: Exposure(4.0),
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("injected save failure"));
        assert_eq!(exchange.submitted_orders.lock().unwrap().len(), 1);

        let instance = current_instance(&state).await;
        assert_eq!(
            instance.pending_order,
            Some(PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: None,
                client_order_id: "intent-1".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: Exposure(4.0),
                status: OrderStatus::Submitting,
            })
        );

        let persisted = persistence
            .snapshots
            .lock()
            .await
            .get("BTCUSDT")
            .cloned()
            .unwrap();
        assert_eq!(
            persisted.pending_order.unwrap().status,
            OrderStatus::Submitting
        );
    }

    #[tokio::test]
    async fn cancel_order_keeps_canceling_intent_when_clear_persistence_fails() {
        let exchange = Arc::new(FakeExchange::new(
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            vec![],
        ));
        let persistence = Arc::new(FailOnSavePersistence::new(2));
        let snapshot = GridSnapshot {
            pending_order: Some(PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("order-1".into()),
                client_order_id: "intent-1".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: Exposure(4.0),
                status: OrderStatus::New,
            }),
            ..test_snapshot()
        };
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone() as Arc<dyn StateRepositoryPort>,
            Some(snapshot.clone()),
            test_budget(),
        )
        .await;
        persistence
            .snapshots
            .lock()
            .await
            .insert("BTCUSDT".into(), snapshot);

        let error = execute_action(
            &state,
            exchange.as_ref(),
            "BTCUSDT",
            "BTCUSDT",
            ExecutionAction::CancelOrder {
                order_id: "order-1".into(),
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("injected save failure"));

        let instance = current_instance(&state).await;
        assert_eq!(
            instance
                .pending_order
                .as_ref()
                .map(|pending| pending.status),
            Some(OrderStatus::Canceling)
        );

        let persisted = persistence
            .snapshots
            .lock()
            .await
            .get("BTCUSDT")
            .cloned()
            .unwrap();
        assert_eq!(
            persisted
                .pending_order
                .as_ref()
                .map(|pending| pending.status),
            Some(OrderStatus::Canceling)
        );
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
            reference_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };
        let outcome = fixture
            .state
            .service
            .mutate_grid("BTCUSDT", |manager| manager.on_price_tick(&tick))
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

        fixture
            .state
            .service
            .mutate_grid("BTCUSDT", |manager| manager.pause_instance("BTCUSDT"))
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
                reference_price: 95.0,
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
        snapshot.reference_price = Some(95.0);

        let fixture = runtime_fixture(
            Some(snapshot),
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
        snapshot.reference_price = Some(100.0);
        snapshot.risk_state.unrealized_pnl = 0.0;

        let fixture = runtime_fixture(
            Some(snapshot),
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
        let mut receiver = fixture.state.service.subscribe_events();
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
        assert_eq!(event.event, ProtocolDomainEvent::SnapshotUpdated);

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
                reference_price: 95.0,
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
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                ExchangeOrder {
                    symbol: "BTCUSDT".into(),
                    order_id: pending.order_id.clone().unwrap(),
                    client_order_id: pending.client_order_id.clone(),
                    side: Side::Buy,
                    price: pending.price,
                    qty: pending.quantity,
                    realized_pnl: 0.0,
                    status: OrderStatus::Filled,
                },
            ))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| instance.pending_order.is_none()).await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn terminal_order_update_reconciles_without_waiting_for_new_tick() {
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
                reference_price: 95.0,
                mark_price: 95.0,
                timestamp: Utc::now(),
            })
            .await
            .unwrap();
        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;

        let pending = current_instance(&fixture.state)
            .await
            .pending_order
            .unwrap();

        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                ExchangeOrder {
                    symbol: "BTCUSDT".into(),
                    order_id: pending.order_id.clone().unwrap(),
                    client_order_id: pending.client_order_id.clone(),
                    side: Side::Buy,
                    price: pending.price,
                    qty: pending.quantity,
                    realized_pnl: 0.0,
                    status: OrderStatus::Canceled,
                },
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
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(0.0)),
            reference_price: Some(100.0),
            pending_order: Some(PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("order-1".into()),
                client_order_id: "order-1".into(),
                side: Side::Buy,
                price: 100.0,
                quantity: 0.1,
                target_exposure: Exposure(0.0),
                status: OrderStatus::New,
            }),
            ..test_snapshot()
        };
        let open_orders = vec![ExchangeOrder {
            symbol: "BTCUSDT".into(),
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
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            open_orders,
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        let mut receiver = fixture.state.service.subscribe_events();
        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                ExchangeOrder {
                    symbol: "BTCUSDT".into(),
                    order_id: "order-1".into(),
                    client_order_id: "order-1".into(),
                    side: Side::Buy,
                    price: 100.0,
                    qty: 0.1,
                    realized_pnl: 0.0,
                    status: OrderStatus::Canceled,
                },
            ))
            .await
            .unwrap();

        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event, ProtocolDomainEvent::SnapshotUpdated);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_uses_live_position_and_open_orders_before_first_tick() {
        let snapshot = test_snapshot();
        let live_order = ExchangeOrder {
            symbol: "BTCUSDT".into(),
            order_id: "live-1".into(),
            client_order_id: "live-1".into(),
            side: Side::Buy,
            price: 94.5,
            qty: 0.25,
            realized_pnl: 0.0,
            status: OrderStatus::New,
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
                reference_price: 92.5,
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
        let fixture = runtime_fixture(
            None,
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
        let exchange = Arc::new(FakeExchange::new(
            Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            },
            vec![],
        ));
        let persistence = Arc::new(FailOnSavePersistence::new(2));
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = InstanceManager::new(clock);
        manager
            .add_grid(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let state = ServerState {
            service: Arc::new(GridPlatformService::new(manager, persistence, events)),
        };
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
    async fn stale_live_user_event_does_not_rollback_state_after_start() {
        let fixture = runtime_fixture(
            None,
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
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                ExchangeOrder {
                    symbol: "BTCUSDT".into(),
                    order_id: "fill-1".into(),
                    client_order_id: "fill-1".into(),
                    side: Side::Sell,
                    price: 95.0,
                    qty: 7.5,
                    realized_pnl: -20.0,
                    status: OrderStatus::Filled,
                },
            ))
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
                reference_price: 95.0,
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

        let mut manager = InstanceManager::new(clock);
        manager
            .add_grid(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let state = ServerState {
            service: Arc::new(GridPlatformService::new(manager, persistence, events)),
        };

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

        let mut manager = InstanceManager::new(clock);
        manager
            .add_grid(
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
                .save_transition("BTCUSDT", &snapshot, &[])
                .await
                .unwrap();
        }

        let (events, _) = broadcast::channel(16);
        let state = ServerState {
            service: Arc::new(GridPlatformService::new(manager, persistence, events)),
        };

        RuntimeFixture {
            runtime: ServerRuntime::new(
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

    async fn test_state(
        exchange: Arc<dyn ExchangePort>,
        persistence: Arc<dyn StateRepositoryPort>,
        restored_snapshot: Option<GridSnapshot>,
        budget: CapacityBudget,
    ) -> ServerState {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = InstanceManager::new(clock);
        manager
            .add_grid(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                test_config(),
                budget,
                exchange.get_exchange_info("BTCUSDT").await.unwrap().rules,
            )
            .unwrap();
        if let Some(snapshot) = restored_snapshot {
            manager.restore_instance_state(&snapshot).unwrap();
        }

        let (events, _) = broadcast::channel(16);
        ServerState {
            service: Arc::new(GridPlatformService::new(manager, persistence, events)),
        }
    }

    async fn current_instance(state: &ServerState) -> grid_engine::instance::StrategyInstance {
        let manager_handle = state.service.manager();
        let manager = manager_handle.read().await;
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

    async fn wait_until_instance<F>(state: &ServerState, predicate: F)
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

    fn position_event_at(
        event_time: chrono::DateTime<Utc>,
        qty: f64,
        unrealized_pnl: f64,
    ) -> UserDataEvent {
        UserDataEvent {
            event_time,
            payload: UserDataPayload::PositionUpdate(Position {
                symbol: "BTCUSDT".into(),
                qty,
                avg_price: 100.0,
                unrealized_pnl,
            }),
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
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            config: test_config(),
            status: GridStatus::Active,
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
                status: OrderStatus::New,
            }),
            risk_state: RiskState::default(),
            reference_price: Some(95.0),
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
        open_orders: Mutex<Vec<ExchangeOrder>>,
        submitted_orders: Mutex<Vec<OrderRequest>>,
        cancel_all_symbols: Mutex<Vec<String>>,
        server_time: chrono::DateTime<Utc>,
        sequence: AtomicUsize,
    }

    impl FakeExchange {
        fn new(position: Position, open_orders: Vec<ExchangeOrder>) -> Self {
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
                server_time: test_server_time(),
                sequence: AtomicUsize::new(1),
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
                status: OrderStatus::New,
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

        async fn get_open_orders(&self, _symbol: &str) -> Result<Vec<ExchangeOrder>> {
            Ok(self.open_orders.lock().unwrap().clone())
        }

        async fn get_exchange_info(&self, _symbol: &str) -> Result<ExchangeInfo> {
            Ok(self.exchange_info.clone())
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(self.server_time)
        }
    }

    #[derive(Default)]
    struct MemoryPersistence {
        snapshots: AsyncMutex<HashMap<String, GridSnapshot>>,
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryPersistence {
        async fn save_transition(
            &self,
            id: &str,
            state: &GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
        ) -> Result<()> {
            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());
            Ok(())
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }
    }

    struct FailOnSavePersistence {
        snapshots: AsyncMutex<HashMap<String, GridSnapshot>>,
        save_count: AtomicUsize,
        fail_on: usize,
    }

    impl FailOnSavePersistence {
        fn new(fail_on: usize) -> Self {
            Self {
                snapshots: AsyncMutex::new(HashMap::new()),
                save_count: AtomicUsize::new(0),
                fail_on,
            }
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailOnSavePersistence {
        async fn save_transition(
            &self,
            id: &str,
            state: &GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
        ) -> Result<()> {
            let save_number = self.save_count.fetch_add(1, Ordering::SeqCst) + 1;
            if save_number == self.fail_on {
                return Err(anyhow!("injected save failure"));
            }

            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());
            Ok(())
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
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
        async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>> {
            self.price_receivers
                .lock()
                .unwrap()
                .remove(symbol)
                .ok_or_else(|| anyhow!("missing test price receiver for {symbol}"))
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
