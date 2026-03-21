use std::{
    future::Future,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{SecondsFormat, Utc};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::background::spawn_task;
use crate::execution::{
    CancelOrdersRequest, ExecutionAdapter, ExecutionMode, ExecutionOutcome, ExecutionRuntimePatch,
    ExecutionStatePatch, FakeExecutionAdapter, PaperFillMarketUpdate, SubmitOrderRequest,
    simulate_paper_fills,
};
use crate::protocol::{
    CommandAccepted, CommandAck, CommandLinks, CommandRecord, CommandRequest, CommandStatus,
    CommandType, ConnectionState, GridLevelState, GridSide, OpenOrder, OpenOrdersSource,
    PROTOCOL_VERSION, PendingCommand, PriceUpdated, RecentFill, RiskEvent, RuntimeSnapshot,
    StrategyStatus, SystemEvent,
};
use crate::storage::{PersistedRuntime, SqliteStorage};
use crate::{risk, strategy};

const ENGINE_COMMAND_BUFFER: usize = 256;
const ENGINE_EVENT_BUFFER: usize = 256;
const EXECUTION_TIMEOUT: Duration = Duration::from_millis(250);
const EXECUTION_MAX_ATTEMPTS: usize = 3;
const EXECUTION_RETRY_DELAY: Duration = Duration::from_millis(20);
const STRATEGY_SYNC_TIMEOUT: Duration = Duration::from_millis(250);

pub type SharedReadModel = Arc<RwLock<ReadModel>>;

#[derive(Debug, Clone)]
pub struct ReadModel {
    snapshot: RuntimeSnapshot,
    risk_events: Vec<RiskEvent>,
    system_events: Vec<SystemEvent>,
    last_sequence: u64,
}

impl ReadModel {
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot.clone()
    }

    pub fn open_orders(&self) -> Vec<crate::protocol::OpenOrder> {
        self.snapshot.execution.open_orders.clone()
    }

    pub fn recent_fills(&self) -> Vec<RecentFill> {
        self.snapshot.execution.recent_fills.clone()
    }

    pub fn risk_events(&self) -> Vec<RiskEvent> {
        self.risk_events.clone()
    }

    pub fn system_events(&self) -> Vec<SystemEvent> {
        self.system_events.clone()
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }
}

#[derive(Debug)]
pub enum EngineCommand {
    SubmitCommand {
        command: CommandType,
        request: CommandRequest,
        reply_to: oneshot::Sender<Result<CommandAccepted>>,
    },
    EmitPriceTick {
        reply_to: oneshot::Sender<Result<PriceUpdated>>,
    },
    SyncConnection {
        connection: ConnectionState,
        reply_to: oneshot::Sender<Result<()>>,
    },
    SyncRuntime {
        patch: RuntimePatch,
        reply_to: oneshot::Sender<Result<()>>,
    },
    SyncExchangeOpenOrders {
        orders: Vec<OpenOrder>,
        source: OpenOrdersSource,
        reply_to: oneshot::Sender<Result<()>>,
    },
    SyncMarketPrices {
        last_price: Option<f64>,
        mark_price: Option<f64>,
        emitted_at: String,
        reply_to: oneshot::Sender<Result<()>>,
    },
    ExecutionFinished {
        command_id: String,
        outcome: ExecutionOutcome,
    },
    ExecutionTimedOut {
        command_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    CommandAck(CommandAck),
    PriceUpdated(PriceUpdated),
    RiskAlert(RiskEvent),
    RuntimeSnapshot(RuntimeSnapshot),
    ConnectionChanged(ConnectionState),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SequencedEngineEvent {
    pub sequence: u64,
    pub event: EngineEvent,
}

#[derive(Debug, Clone)]
pub struct EngineHandle {
    commands_tx: mpsc::Sender<EngineCommand>,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimePatch {
    pub symbol: Option<String>,
    pub env: Option<String>,
    pub session_state: Option<String>,
    pub position_qty: Option<f64>,
    pub position_avg_price: Option<f64>,
    pub unrealized_pnl: Option<f64>,
    pub realized_pnl: Option<f64>,
}

impl EngineHandle {
    pub async fn submit_command(
        &self,
        command: CommandType,
        request: CommandRequest,
    ) -> Result<CommandAccepted> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(EngineCommand::SubmitCommand {
                command,
                request,
                reply_to,
            })
            .await
            .context("failed to enqueue engine command")?;
        let result = reply_rx
            .await
            .context("engine loop dropped before acknowledging command")?;
        result
    }

    pub async fn emit_price_tick(&self) -> Result<PriceUpdated> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(EngineCommand::EmitPriceTick { reply_to })
            .await
            .context("failed to enqueue price tick")?;
        let result = reply_rx
            .await
            .context("engine loop dropped before emitting price tick")?;
        result
    }

    pub(crate) async fn sync_connection(&self, connection: ConnectionState) -> Result<()> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(EngineCommand::SyncConnection {
                connection,
                reply_to,
            })
            .await
            .context("failed to enqueue connection sync")?;
        let result = reply_rx
            .await
            .context("engine loop dropped before syncing connection state")?;
        result
    }

    pub(crate) async fn sync_runtime(&self, patch: RuntimePatch) -> Result<()> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(EngineCommand::SyncRuntime { patch, reply_to })
            .await
            .context("failed to enqueue runtime sync")?;
        let result = reply_rx
            .await
            .context("engine loop dropped before syncing runtime state")?;
        result
    }

    pub(crate) async fn sync_exchange_open_orders(
        &self,
        orders: Vec<OpenOrder>,
        source: OpenOrdersSource,
    ) -> Result<()> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(EngineCommand::SyncExchangeOpenOrders {
                orders,
                source,
                reply_to,
            })
            .await
            .context("failed to enqueue exchange open orders sync")?;
        let result = reply_rx
            .await
            .context("engine loop dropped before syncing exchange open orders")?;
        result
    }

    pub(crate) async fn sync_market_prices(
        &self,
        last_price: Option<f64>,
        mark_price: Option<f64>,
        emitted_at: String,
    ) -> Result<()> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(EngineCommand::SyncMarketPrices {
                last_price,
                mark_price,
                emitted_at,
                reply_to,
            })
            .await
            .context("failed to enqueue market price sync")?;
        let result = reply_rx
            .await
            .context("engine loop dropped before syncing market prices")?;
        result
    }
}

pub fn spawn_engine() -> (
    EngineHandle,
    SharedReadModel,
    mpsc::Receiver<SequencedEngineEvent>,
) {
    spawn_engine_with_runtime_and_adapter(
        PersistedRuntime::in_memory_bootstrap(),
        None,
        Arc::new(FakeExecutionAdapter),
    )
}

pub fn spawn_engine_with_adapter(
    execution_adapter: Arc<dyn ExecutionAdapter>,
) -> (
    EngineHandle,
    SharedReadModel,
    mpsc::Receiver<SequencedEngineEvent>,
) {
    spawn_engine_with_runtime_and_adapter(
        PersistedRuntime::in_memory_bootstrap(),
        None,
        execution_adapter,
    )
}

pub fn spawn_engine_with_runtime(
    runtime: PersistedRuntime,
    storage: Option<SqliteStorage>,
) -> (
    EngineHandle,
    SharedReadModel,
    mpsc::Receiver<SequencedEngineEvent>,
) {
    spawn_engine_with_runtime_and_adapter(runtime, storage, Arc::new(FakeExecutionAdapter))
}

pub fn spawn_engine_with_runtime_and_adapter(
    runtime: PersistedRuntime,
    storage: Option<SqliteStorage>,
    execution_adapter: Arc<dyn ExecutionAdapter>,
) -> (
    EngineHandle,
    SharedReadModel,
    mpsc::Receiver<SequencedEngineEvent>,
) {
    let aggregate = RuntimeAggregate::from_persisted(runtime);
    let read_model = Arc::new(RwLock::new(ReadModel::from(&aggregate)));
    let (commands_tx, commands_rx) = mpsc::channel(ENGINE_COMMAND_BUFFER);
    let (events_tx, events_rx) = mpsc::channel(ENGINE_EVENT_BUFFER);

    spawn_task(run_engine(
        commands_tx.clone(),
        commands_rx,
        events_tx,
        read_model.clone(),
        aggregate,
        storage,
        execution_adapter,
    ));

    (EngineHandle { commands_tx }, read_model, events_rx)
}

async fn run_engine(
    commands_tx: mpsc::Sender<EngineCommand>,
    mut commands_rx: mpsc::Receiver<EngineCommand>,
    events_tx: mpsc::Sender<SequencedEngineEvent>,
    read_model: SharedReadModel,
    mut aggregate: RuntimeAggregate,
    storage: Option<SqliteStorage>,
    execution_adapter: Arc<dyn ExecutionAdapter>,
) {
    let mut active_execution: Option<ActiveExecutionTask> = None;
    while let Some(command) = commands_rx.recv().await {
        match command {
            EngineCommand::SubmitCommand {
                command,
                request,
                reply_to,
            } => {
                let previous = aggregate.clone();
                let issued = match aggregate.issue_command(command, request, storage.as_ref()) {
                    Ok(value) => value,
                    Err(error) => {
                        aggregate = previous;
                        let _ = reply_to.send(Err(error));
                        continue;
                    }
                };
                let strategy_events = match &issued {
                    IssuedCommand::Immediate { .. }
                        if should_sync_strategy_after_command(command) =>
                    {
                        match maybe_sync_strategy_orders(&mut aggregate, execution_adapter.clone())
                            .await
                        {
                            Ok(events) => events,
                            Err(error) => {
                                aggregate = previous.clone();
                                let error = error.context("failed to sync strategy orders");
                                warn!(?error, "failed to sync strategy orders; reverting command");
                                let _ = reply_to.send(Err(error));
                                continue;
                            }
                        }
                    }
                    _ => Vec::new(),
                };
                if let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate) {
                    aggregate = previous;
                    let error = error.context("failed to persist command result");
                    warn!(
                        ?error,
                        "failed to persist runtime state to sqlite; reverting command"
                    );
                    let _ = reply_to.send(Err(error));
                    continue;
                }
                replace_read_model(&read_model, &aggregate);
                match issued {
                    IssuedCommand::Immediate {
                        accepted,
                        mut events,
                    } => {
                        events.extend(strategy_events);
                        let _ = reply_to.send(Ok(accepted));
                        publish_events(&events_tx, events).await;
                    }
                    IssuedCommand::Deferred { accepted, launch } => {
                        let execution_adapter = execution_adapter.clone();
                        let commands_tx = commands_tx.clone();
                        let timeout_tx = commands_tx.clone();
                        let timeout_command_id = launch.command_id.clone();
                        let task_command_id = launch.command_id.clone();
                        let execution_task = tokio::spawn(async move {
                            let outcome =
                                match run_deferred_execution(execution_adapter, &launch).await {
                                    Ok(outcome) => outcome,
                                    Err(error) => ExecutionOutcome::failed(error.to_string()),
                                };
                            if commands_tx
                                .send(EngineCommand::ExecutionFinished {
                                    command_id: launch.command_id,
                                    outcome,
                                })
                                .await
                                .is_err()
                            {
                                warn!("engine command channel closed while finishing execution");
                            }
                        });
                        active_execution = Some(ActiveExecutionTask {
                            command_id: task_command_id,
                            task: execution_task,
                        });
                        spawn_task(async move {
                            tokio::time::sleep(EXECUTION_TIMEOUT).await;
                            if timeout_tx
                                .send(EngineCommand::ExecutionTimedOut {
                                    command_id: timeout_command_id,
                                })
                                .await
                                .is_err()
                            {
                                warn!("engine command channel closed while timing out execution");
                            }
                        });
                        let _ = reply_to.send(Ok(accepted));
                    }
                }
            }
            EngineCommand::EmitPriceTick { reply_to } => {
                let previous = aggregate.clone();
                let (tick, mut events) =
                    aggregate.emit_price_tick(execution_adapter.mode() == ExecutionMode::Paper);
                match maybe_sync_strategy_orders(&mut aggregate, execution_adapter.clone()).await {
                    Ok(strategy_events) => events.extend(strategy_events),
                    Err(error) => {
                        aggregate = previous.clone();
                        let error =
                            error.context("failed to sync strategy orders after price tick");
                        warn!(
                            ?error,
                            "failed to sync strategy orders after price tick; reverting price update"
                        );
                        let _ = reply_to.send(Err(error));
                        continue;
                    }
                }
                if let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate) {
                    aggregate = previous;
                    let error = error.context("failed to persist price update");
                    warn!(
                        ?error,
                        "failed to persist runtime state to sqlite; reverting price update"
                    );
                    let _ = reply_to.send(Err(error));
                    continue;
                }
                replace_read_model(&read_model, &aggregate);
                let _ = reply_to.send(Ok(tick.clone()));
                publish_events(&events_tx, events).await;
            }
            EngineCommand::SyncConnection {
                connection,
                reply_to,
            } => {
                let previous = aggregate.clone();
                let events = aggregate.sync_connection(connection);
                if !events.is_empty()
                    && let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate)
                {
                    aggregate = previous;
                    let error = error.context("failed to persist connection state");
                    warn!(
                        ?error,
                        "failed to persist runtime state to sqlite; reverting connection change"
                    );
                    let _ = reply_to.send(Err(error));
                    continue;
                }
                replace_read_model(&read_model, &aggregate);
                let _ = reply_to.send(Ok(()));
                publish_events(&events_tx, events).await;
            }
            EngineCommand::SyncRuntime { patch, reply_to } => {
                let previous = aggregate.clone();
                let mut events = aggregate.sync_runtime(patch);
                match maybe_sync_strategy_orders(&mut aggregate, execution_adapter.clone()).await {
                    Ok(strategy_events) => events.extend(strategy_events),
                    Err(error) => {
                        aggregate = previous.clone();
                        let error =
                            error.context("failed to sync strategy orders after runtime sync");
                        warn!(
                            ?error,
                            "failed to sync strategy orders after runtime sync; reverting runtime sync"
                        );
                        let _ = reply_to.send(Err(error));
                        continue;
                    }
                }
                if !events.is_empty()
                    && let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate)
                {
                    aggregate = previous;
                    let error = error.context("failed to persist runtime sync");
                    warn!(
                        ?error,
                        "failed to persist runtime state to sqlite; reverting runtime sync"
                    );
                    let _ = reply_to.send(Err(error));
                    continue;
                }
                replace_read_model(&read_model, &aggregate);
                let _ = reply_to.send(Ok(()));
                publish_events(&events_tx, events).await;
            }
            EngineCommand::SyncExchangeOpenOrders {
                orders,
                source,
                reply_to,
            } => {
                let previous = aggregate.clone();
                let events = aggregate.sync_exchange_open_orders(orders, source);
                if !events.is_empty()
                    && let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate)
                {
                    aggregate = previous;
                    let error = error.context("failed to persist exchange open orders sync");
                    warn!(
                        ?error,
                        "failed to persist runtime state to sqlite; reverting exchange open orders sync"
                    );
                    let _ = reply_to.send(Err(error));
                    continue;
                }
                replace_read_model(&read_model, &aggregate);
                let _ = reply_to.send(Ok(()));
                publish_events(&events_tx, events).await;
            }
            EngineCommand::SyncMarketPrices {
                last_price,
                mark_price,
                emitted_at,
                reply_to,
            } => {
                let previous = aggregate.clone();
                let mut events = aggregate.sync_market_prices(
                    last_price,
                    mark_price,
                    emitted_at,
                    execution_adapter.mode() == ExecutionMode::Paper,
                );
                match maybe_sync_strategy_orders(&mut aggregate, execution_adapter.clone()).await {
                    Ok(strategy_events) => events.extend(strategy_events),
                    Err(error) => {
                        aggregate = previous.clone();
                        let error =
                            error.context("failed to sync strategy orders after market price sync");
                        warn!(
                            ?error,
                            "failed to sync strategy orders after market price sync; reverting market price sync"
                        );
                        let _ = reply_to.send(Err(error));
                        continue;
                    }
                }
                if !events.is_empty()
                    && let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate)
                {
                    aggregate = previous;
                    let error = error.context("failed to persist market price sync");
                    warn!(
                        ?error,
                        "failed to persist runtime state to sqlite; reverting market price sync"
                    );
                    let _ = reply_to.send(Err(error));
                    continue;
                }
                replace_read_model(&read_model, &aggregate);
                let _ = reply_to.send(Ok(()));
                publish_events(&events_tx, events).await;
            }
            EngineCommand::ExecutionFinished {
                command_id,
                outcome,
            } => {
                if active_execution
                    .as_ref()
                    .is_some_and(|active| active.command_id == command_id)
                {
                    active_execution = None;
                }
                let previous = aggregate.clone();
                let events = aggregate.finish_execution(&command_id, outcome);
                if events.is_empty() {
                    continue;
                }
                if let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate) {
                    aggregate = previous;
                    let error = error.context("failed to persist execution result");
                    warn!(
                        ?error,
                        "failed to persist runtime state to sqlite; reverting execution result"
                    );
                    continue;
                }
                replace_read_model(&read_model, &aggregate);
                publish_events(&events_tx, events).await;
            }
            EngineCommand::ExecutionTimedOut { command_id } => {
                if active_execution
                    .as_ref()
                    .is_some_and(|active| active.command_id == command_id)
                    && let Some(active) = active_execution.take()
                {
                    active.task.abort();
                }
                let previous = aggregate.clone();
                let events = aggregate.timeout_execution(&command_id);
                if events.is_empty() {
                    continue;
                }
                if let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate) {
                    aggregate = previous;
                    let error = error.context("failed to persist execution timeout");
                    warn!(
                        ?error,
                        "failed to persist runtime state to sqlite; reverting execution timeout"
                    );
                    continue;
                }
                replace_read_model(&read_model, &aggregate);
                publish_events(&events_tx, events).await;
            }
        }
    }
}

async fn publish_events(
    events_tx: &mpsc::Sender<SequencedEngineEvent>,
    events: Vec<SequencedEngineEvent>,
) {
    for event in events {
        if events_tx.send(event).await.is_err() {
            warn!("engine event channel closed while publishing engine event");
            break;
        }
    }
}

fn replace_read_model(read_model: &SharedReadModel, aggregate: &RuntimeAggregate) {
    *read_model
        .write()
        .expect("service read model rwlock poisoned") = ReadModel::from(aggregate);
}

async fn run_deferred_execution(
    execution_adapter: Arc<dyn ExecutionAdapter>,
    launch: &DeferredExecution,
) -> Result<ExecutionOutcome> {
    match launch.command {
        CommandType::CancelAll => execute_cancel_all(execution_adapter, launch).await,
        CommandType::FlattenNow => execute_flatten(execution_adapter, launch, false).await,
        CommandType::ShutdownAfterFlatten => execute_flatten(execution_adapter, launch, true).await,
        CommandType::Pause | CommandType::Resume => {
            unreachable!("local runtime commands do not use deferred execution")
        }
    }
}

async fn execute_cancel_all(
    execution_adapter: Arc<dyn ExecutionAdapter>,
    launch: &DeferredExecution,
) -> Result<ExecutionOutcome> {
    let request = CancelOrdersRequest {
        command_id: Some(launch.command_id.clone()),
        order_ids: launch
            .snapshot
            .execution
            .open_orders
            .iter()
            .map(|order| order.order_id.clone())
            .collect(),
        client_order_ids: launch
            .snapshot
            .execution
            .open_orders
            .iter()
            .map(|order| order.client_order_id.clone())
            .collect(),
    };
    let mut working_snapshot = launch.snapshot.clone();
    working_snapshot.execution.open_orders =
        retry_execution(|| execution_adapter.cancel_orders(request.clone(), &working_snapshot))
            .await?;

    let mut outcome =
        if targeted_open_orders_still_present(&working_snapshot.execution.open_orders, &request) {
            ExecutionOutcome::failed("Cancel-all did not clear all targeted open orders.")
        } else {
            ExecutionOutcome::completed("All open orders cancelled.")
        };
    outcome.links.client_order_ids = request.client_order_ids;
    outcome.links.order_ids = request.order_ids;
    outcome.open_orders = Some(working_snapshot.execution.open_orders.clone());
    outcome.recent_fills = Some(working_snapshot.execution.recent_fills.clone());
    Ok(outcome)
}

async fn execute_flatten(
    execution_adapter: Arc<dyn ExecutionAdapter>,
    launch: &DeferredExecution,
    pause_after_flatten: bool,
) -> Result<ExecutionOutcome> {
    let mut working_snapshot = launch.snapshot.clone();
    let mut cancelled_links = CommandLinks::default();

    if pause_after_flatten {
        let cancel_request = CancelOrdersRequest {
            command_id: Some(launch.command_id.clone()),
            order_ids: working_snapshot
                .execution
                .open_orders
                .iter()
                .map(|order| order.order_id.clone())
                .collect(),
            client_order_ids: working_snapshot
                .execution
                .open_orders
                .iter()
                .map(|order| order.client_order_id.clone())
                .collect(),
        };
        working_snapshot.execution.open_orders = retry_execution(|| {
            execution_adapter.cancel_orders(cancel_request.clone(), &working_snapshot)
        })
        .await?;
        if targeted_open_orders_still_present(
            &working_snapshot.execution.open_orders,
            &cancel_request,
        ) {
            let mut outcome =
                ExecutionOutcome::failed("Shutdown-after-flatten did not clear all open orders.");
            outcome.links.client_order_ids = cancel_request.client_order_ids;
            outcome.links.order_ids = cancel_request.order_ids;
            outcome.open_orders = Some(working_snapshot.execution.open_orders.clone());
            outcome.recent_fills = Some(working_snapshot.execution.recent_fills.clone());
            return Ok(outcome);
        }
        cancelled_links.client_order_ids = cancel_request.client_order_ids;
        cancelled_links.order_ids = cancel_request.order_ids;
    }

    let qty = working_snapshot.runtime.position_qty.abs();
    let mut flatten_links = CommandLinks::default();
    let mut flatten_realized_pnl = None;
    if qty > f64::EPSILON {
        let submit_request = SubmitOrderRequest {
            command_id: Some(launch.command_id.clone()),
            order_id: format!("order_{}", launch.command_id),
            client_order_id: format!("reduce_only_{}", launch.command_id),
            side: if working_snapshot.runtime.position_qty > 0.0 {
                "sell".into()
            } else {
                "buy".into()
            },
            price: if working_snapshot.runtime.mark_price > 0.0 {
                working_snapshot.runtime.mark_price
            } else {
                working_snapshot.runtime.last_price
            },
            qty,
            reduce_only: true,
        };
        let submitted = retry_execution(|| {
            execution_adapter.submit_order(submit_request.clone(), &working_snapshot)
        })
        .await?;
        apply_submit_result(&mut working_snapshot, submitted.clone());
        flatten_links
            .client_order_ids
            .push(submit_request.client_order_id.clone());
        flatten_links
            .order_ids
            .push(submit_request.order_id.clone());
        if let Some(fill) = submitted.fill {
            flatten_realized_pnl = Some(fill.realized_pnl);
            flatten_links.trade_ids.push(fill.trade_id);
        }

        if !flatten_terminal_state_observed(&working_snapshot, &submit_request) {
            let mut outcome =
                ExecutionOutcome::failed("Flatten order did not produce a terminal fill.");
            outcome.links = combine_links(cancelled_links, flatten_links);
            outcome.open_orders = Some(working_snapshot.execution.open_orders.clone());
            outcome.recent_fills = Some(working_snapshot.execution.recent_fills.clone());
            return Ok(outcome);
        }
    }

    let mut outcome = ExecutionOutcome::completed(if pause_after_flatten {
        "Position flattened and shutdown requested."
    } else {
        "Position flattened."
    });
    outcome.links = combine_links(cancelled_links, flatten_links);
    outcome.open_orders = Some(working_snapshot.execution.open_orders.clone());
    outcome.recent_fills = Some(working_snapshot.execution.recent_fills.clone());
    if pause_after_flatten {
        outcome.runtime_patch.strategy_state = Some("paused".into());
    }
    outcome.runtime_patch.position_qty = Some(0.0);
    outcome.runtime_patch.position_avg_price = Some(0.0);
    outcome.runtime_patch.unrealized_pnl = Some(0.0);
    if let Some(realized_pnl) = flatten_realized_pnl {
        outcome.runtime_patch.realized_pnl =
            Some(working_snapshot.runtime.realized_pnl + realized_pnl);
    }
    Ok(outcome)
}

async fn maybe_sync_strategy_orders(
    aggregate: &mut RuntimeAggregate,
    execution_adapter: Arc<dyn ExecutionAdapter>,
) -> Result<Vec<SequencedEngineEvent>> {
    if aggregate
        .snapshot
        .execution
        .pending_commands
        .iter()
        .any(|item| is_execution_command(item.command))
    {
        return Ok(Vec::new());
    }

    let before = aggregate.snapshot.clone();
    let mut working_snapshot = aggregate.snapshot.clone();
    if let Some(cancel_request) = strategy_orders_to_cancel(&working_snapshot) {
        working_snapshot.execution.open_orders = retry_execution(|| {
            execution_adapter.cancel_orders(cancel_request.clone(), &working_snapshot)
        })
        .await?;
        if targeted_open_orders_still_present(&working_snapshot.execution.open_orders, &cancel_request)
        {
            return Err(anyhow!(
                "strategy waiting state did not clear all targeted strategy orders"
            ));
        }
    }

    let missing_orders = strategy_orders_to_place(&working_snapshot);
    if missing_orders.is_empty() {
        aggregate.snapshot.execution.open_orders = working_snapshot.execution.open_orders.clone();
        aggregate.snapshot.execution.recent_fills = working_snapshot.execution.recent_fills.clone();

        if aggregate.snapshot == before {
            return Ok(Vec::new());
        }

        return Ok(vec![aggregate.sequenced_event(EngineEvent::RuntimeSnapshot(
            aggregate.snapshot.clone(),
        ))]);
    }

    let deadline = tokio::time::Instant::now() + STRATEGY_SYNC_TIMEOUT;
    let mut applied_any = false;
    for order in missing_orders {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            if applied_any {
                warn!(
                    "strategy sync budget exhausted after partial success; keeping applied placements"
                );
                break;
            }
            return Err(anyhow!(
                "strategy placement timed out while waiting for adapter response"
            ));
        };
        match strategy_sync_call(
            remaining,
            execution_adapter.submit_order(order.clone(), &working_snapshot),
        )
        .await
        {
            Ok(submitted) if submit_result_has_facts(&submitted) => {
                apply_submit_result(&mut working_snapshot, submitted);
                applied_any = true;
            }
            Ok(_) if applied_any => {
                warn!(
                    client_order_id = %order.client_order_id,
                    "strategy sync submit returned no execution facts after partial success; keeping applied placements"
                );
                break;
            }
            Ok(_) => {
                return Err(anyhow!(
                    "strategy placement returned no execution facts for {}",
                    order.client_order_id
                ));
            }
            Err(error) if applied_any => {
                warn!(
                    ?error,
                    client_order_id = %order.client_order_id,
                    "strategy sync stopped after partial success; keeping applied placements"
                );
                break;
            }
            Err(error) => return Err(error),
        }
    }
    aggregate.snapshot.execution.open_orders = working_snapshot.execution.open_orders.clone();
    aggregate.snapshot.execution.recent_fills = working_snapshot.execution.recent_fills.clone();

    if aggregate.snapshot == before {
        return Ok(Vec::new());
    }

    Ok(vec![aggregate.sequenced_event(
        EngineEvent::RuntimeSnapshot(aggregate.snapshot.clone()),
    )])
}

async fn retry_execution<T, F, Fut>(mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;
    for attempt in 0..EXECUTION_MAX_ATTEMPTS {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < EXECUTION_MAX_ATTEMPTS {
                    tokio::time::sleep(EXECUTION_RETRY_DELAY).await;
                }
            }
        }
    }
    Err(last_error.expect("retry loop should capture at least one error"))
}

async fn strategy_sync_call<T, Fut>(timeout_after: Duration, future: Fut) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    tokio::time::timeout(timeout_after, future)
        .await
        .map_err(|_| anyhow!("strategy placement timed out while waiting for adapter response"))?
}

fn apply_submit_result(
    snapshot: &mut RuntimeSnapshot,
    submitted: crate::execution::SubmitOrderResult,
) {
    if let Some(open_order) = submitted.open_order {
        upsert_open_order(&mut snapshot.execution.open_orders, open_order);
    }
    if let Some(fill) = submitted.fill {
        snapshot.execution.recent_fills.insert(0, fill);
    }
}

fn submit_result_has_facts(submitted: &crate::execution::SubmitOrderResult) -> bool {
    submitted.open_order.is_some() || submitted.fill.is_some()
}

fn upsert_open_order(open_orders: &mut Vec<OpenOrder>, open_order: OpenOrder) {
    if let Some(index) = open_orders.iter().position(|current| {
        current.order_id == open_order.order_id
            || current.client_order_id == open_order.client_order_id
    }) {
        open_orders[index] = open_order;
    } else {
        open_orders.push(open_order);
    }
}

fn strategy_orders_to_place(snapshot: &RuntimeSnapshot) -> Vec<SubmitOrderRequest> {
    if snapshot.runtime.strategy_state != "running"
        || !has_valid_market_price(snapshot)
        || snapshot.risk.breaker_engaged
        || matches!(
            snapshot.strategy.status,
            StrategyStatus::WaitingMarketPrice
                | StrategyStatus::WaitingRangeEntry
                | StrategyStatus::PendingRebuild
        )
        || (snapshot.strategy.status == StrategyStatus::Occupied
            && snapshot.strategy.status_reason.is_some())
        || snapshot
            .execution
            .open_orders
            .iter()
            .any(|order| order.client_order_id.starts_with("reduce_only_"))
    {
        return Vec::new();
    }

    snapshot
        .strategy
        .levels
        .iter()
        .filter(|level| level.state == GridLevelState::Active)
        .filter_map(|level| {
            let client_order_id = level.client_order_id.clone()?;
            let order_id = level.order_id.clone()?;
            if snapshot
                .execution
                .open_orders
                .iter()
                .any(|order| order.client_order_id == client_order_id || order.order_id == order_id)
            {
                return None;
            }
            Some(SubmitOrderRequest {
                command_id: None,
                order_id,
                client_order_id,
                side: match level.side {
                    GridSide::Buy => "buy".into(),
                    GridSide::Sell => "sell".into(),
                },
                price: level.price,
                qty: level.quantity,
                reduce_only: false,
            })
        })
        .collect()
}

fn strategy_orders_to_cancel(snapshot: &RuntimeSnapshot) -> Option<CancelOrdersRequest> {
    if snapshot.runtime.strategy_state != "running"
        || snapshot.strategy.status != StrategyStatus::WaitingRangeEntry
        || snapshot.runtime.position_qty.abs() > f64::EPSILON
    {
        return None;
    }

    let strategy_orders = snapshot
        .execution
        .open_orders
        .iter()
        .filter(|order| order.client_order_id.starts_with("grid_"))
        .collect::<Vec<_>>();
    if strategy_orders.is_empty() {
        return None;
    }

    Some(CancelOrdersRequest {
        command_id: None,
        order_ids: strategy_orders
            .iter()
            .map(|order| order.order_id.clone())
            .collect(),
        client_order_ids: strategy_orders
            .iter()
            .map(|order| order.client_order_id.clone())
            .collect(),
    })
}

fn has_valid_market_price(snapshot: &RuntimeSnapshot) -> bool {
    snapshot.runtime.mark_price.abs() > f64::EPSILON
        || snapshot.runtime.last_price.abs() > f64::EPSILON
}

fn should_skip_paper_fill_simulation(snapshot: &RuntimeSnapshot) -> bool {
    snapshot.runtime.strategy_state == "running"
        && snapshot.runtime.position_qty.abs() <= f64::EPSILON
        && strategy::reconcile(&snapshot.runtime, &snapshot.risk, &snapshot.strategy).status
            == StrategyStatus::WaitingRangeEntry
}

fn should_sync_strategy_after_command(command: CommandType) -> bool {
    matches!(command, CommandType::Resume)
}

fn targeted_open_orders_still_present(
    open_orders: &[OpenOrder],
    request: &CancelOrdersRequest,
) -> bool {
    open_orders.iter().any(|order| {
        request.order_ids.iter().any(|id| id == &order.order_id)
            || request
                .client_order_ids
                .iter()
                .any(|id| id == &order.client_order_id)
    })
}

fn flatten_terminal_state_observed(
    snapshot: &RuntimeSnapshot,
    request: &SubmitOrderRequest,
) -> bool {
    let filled_qty = snapshot
        .execution
        .recent_fills
        .iter()
        .filter(|fill| {
            fill.order_id == request.order_id
                || fill.client_order_id.as_deref() == Some(request.client_order_id.as_str())
        })
        .map(|fill| fill.qty)
        .sum::<f64>();
    filled_qty + f64::EPSILON >= request.qty
        && snapshot.execution.open_orders.iter().all(|order| {
            order.order_id != request.order_id && order.client_order_id != request.client_order_id
        })
}

fn combine_links(left: CommandLinks, right: CommandLinks) -> CommandLinks {
    let mut links = CommandLinks::default();
    links.client_order_ids.extend(left.client_order_ids);
    links.client_order_ids.extend(right.client_order_ids);
    links.order_ids.extend(left.order_ids);
    links.order_ids.extend(right.order_ids);
    links.trade_ids.extend(left.trade_ids);
    links.trade_ids.extend(right.trade_ids);
    links
}

#[derive(Debug, Clone)]
struct RuntimeAggregate {
    snapshot: RuntimeSnapshot,
    risk_events: Vec<RiskEvent>,
    system_events: Vec<SystemEvent>,
    last_sequence: u64,
}

#[derive(Debug)]
enum IssuedCommand {
    Immediate {
        accepted: CommandAccepted,
        events: Vec<SequencedEngineEvent>,
    },
    Deferred {
        accepted: CommandAccepted,
        launch: DeferredExecution,
    },
}

#[derive(Debug)]
struct DeferredExecution {
    command: CommandType,
    command_id: String,
    snapshot: RuntimeSnapshot,
}

struct ActiveExecutionTask {
    command_id: String,
    task: tokio::task::JoinHandle<()>,
}

struct ReconcileOutcome {
    snapshot_changed: bool,
    risk_alerts: Vec<RiskEvent>,
}

impl RuntimeAggregate {
    fn from_persisted(runtime: PersistedRuntime) -> Self {
        let mut aggregate = Self {
            snapshot: runtime.snapshot,
            risk_events: runtime.risk_events,
            system_events: runtime.system_events,
            last_sequence: runtime.last_sequence,
        };
        let _ = aggregate.reconcile_runtime();
        aggregate
    }

    fn emit_price_tick(&mut self, paper_mode: bool) -> (PriceUpdated, Vec<SequencedEngineEvent>) {
        let emitted_at = now_utc();
        self.snapshot.runtime.last_price = round_price(self.snapshot.runtime.last_price + 0.11);
        self.snapshot.runtime.mark_price = round_price(self.snapshot.runtime.mark_price + 0.08);
        self.snapshot.connection.last_heartbeat_at = emitted_at.clone();
        self.snapshot.connection.stale_age_ms = 0;
        if paper_mode && !should_skip_paper_fill_simulation(&self.snapshot) {
            let patch = simulate_paper_fills(
                &self.snapshot,
                PaperFillMarketUpdate {
                    last_price: Some(self.snapshot.runtime.last_price),
                    mark_price: Some(self.snapshot.runtime.mark_price),
                },
                &emitted_at,
            );
            self.apply_execution_patch(&patch);
        }
        let tick = PriceUpdated {
            symbol: self.snapshot.runtime.symbol.clone(),
            last_price: self.snapshot.runtime.last_price,
            mark_price: self.snapshot.runtime.mark_price,
            emitted_at,
        };
        let reconcile = self.reconcile_runtime();
        let mut events = vec![self.sequenced_event(EngineEvent::PriceUpdated(tick.clone()))];
        events.extend(self.risk_alert_events(reconcile.risk_alerts));
        if reconcile.snapshot_changed {
            events.push(self.sequenced_event(EngineEvent::RuntimeSnapshot(self.snapshot.clone())));
        }
        (tick, events)
    }

    fn sync_connection(&mut self, connection: ConnectionState) -> Vec<SequencedEngineEvent> {
        if self.snapshot.connection == connection {
            return Vec::new();
        }

        self.snapshot.connection = connection.clone();
        vec![self.sequenced_event(EngineEvent::ConnectionChanged(connection))]
    }

    fn sync_runtime(&mut self, patch: RuntimePatch) -> Vec<SequencedEngineEvent> {
        let mut changed = false;

        if let Some(symbol) = patch.symbol
            && self.snapshot.runtime.symbol != symbol
        {
            self.snapshot.runtime.symbol = symbol;
            changed = true;
        }
        if let Some(env) = patch.env
            && self.snapshot.runtime.env != env
        {
            self.snapshot.runtime.env = env;
            changed = true;
        }
        if let Some(session_state) = patch.session_state
            && self.snapshot.runtime.session_state != session_state
        {
            self.snapshot.runtime.session_state = session_state;
            changed = true;
        }
        if let Some(position_qty) = patch.position_qty
            && (self.snapshot.runtime.position_qty - position_qty).abs() > f64::EPSILON
        {
            self.snapshot.runtime.position_qty = position_qty;
            changed = true;
        }
        if let Some(position_avg_price) = patch.position_avg_price
            && (self.snapshot.runtime.position_avg_price - position_avg_price).abs() > f64::EPSILON
        {
            self.snapshot.runtime.position_avg_price = position_avg_price;
            changed = true;
        }
        if let Some(unrealized_pnl) = patch.unrealized_pnl
            && (self.snapshot.runtime.unrealized_pnl - unrealized_pnl).abs() > f64::EPSILON
        {
            self.snapshot.runtime.unrealized_pnl = unrealized_pnl;
            changed = true;
        }
        if let Some(realized_pnl) = patch.realized_pnl
            && (self.snapshot.runtime.realized_pnl - realized_pnl).abs() > f64::EPSILON
        {
            self.snapshot.runtime.realized_pnl = realized_pnl;
            changed = true;
        }

        if !changed {
            return Vec::new();
        }

        let reconcile = self.reconcile_runtime();
        let mut events =
            vec![self.sequenced_event(EngineEvent::RuntimeSnapshot(self.snapshot.clone()))];
        events.extend(self.risk_alert_events(reconcile.risk_alerts));
        events
    }

    fn sync_exchange_open_orders(
        &mut self,
        orders: Vec<OpenOrder>,
        source: OpenOrdersSource,
    ) -> Vec<SequencedEngineEvent> {
        if self.snapshot.execution.exchange_open_orders == orders
            && self.snapshot.execution.exchange_open_orders_source == source
        {
            return Vec::new();
        }

        self.snapshot.execution.exchange_open_orders = orders;
        self.snapshot.execution.exchange_open_orders_source = source;
        vec![self.sequenced_event(EngineEvent::RuntimeSnapshot(self.snapshot.clone()))]
    }

    fn sync_market_prices(
        &mut self,
        last_price: Option<f64>,
        mark_price: Option<f64>,
        emitted_at: String,
        paper_mode: bool,
    ) -> Vec<SequencedEngineEvent> {
        let mut changed = false;

        if let Some(last_price) = last_price {
            if (self.snapshot.runtime.last_price - last_price).abs() > f64::EPSILON {
                self.snapshot.runtime.last_price = last_price;
                changed = true;
            }
        }
        if let Some(mark_price) = mark_price {
            if (self.snapshot.runtime.mark_price - mark_price).abs() > f64::EPSILON {
                self.snapshot.runtime.mark_price = mark_price;
                changed = true;
            }
        }

        if self.snapshot.connection.last_heartbeat_at != emitted_at {
            self.snapshot.connection.last_heartbeat_at = emitted_at.clone();
            changed = true;
        }
        if self.snapshot.connection.stale_age_ms != 0 {
            self.snapshot.connection.stale_age_ms = 0;
            changed = true;
        }

        if paper_mode && !should_skip_paper_fill_simulation(&self.snapshot) {
            let patch = simulate_paper_fills(
                &self.snapshot,
                PaperFillMarketUpdate {
                    last_price,
                    mark_price,
                },
                &emitted_at,
            );
            changed |= self.apply_execution_patch(&patch);
        }

        if !changed {
            return Vec::new();
        }

        let tick = PriceUpdated {
            symbol: self.snapshot.runtime.symbol.clone(),
            last_price: self.snapshot.runtime.last_price,
            mark_price: self.snapshot.runtime.mark_price,
            emitted_at,
        };
        let reconcile = self.reconcile_runtime();
        let mut events = vec![self.sequenced_event(EngineEvent::PriceUpdated(tick))];
        events.extend(self.risk_alert_events(reconcile.risk_alerts));
        if reconcile.snapshot_changed {
            events.push(self.sequenced_event(EngineEvent::RuntimeSnapshot(self.snapshot.clone())));
        }
        events
    }

    fn issue_command(
        &mut self,
        command: CommandType,
        request: CommandRequest,
        storage: Option<&SqliteStorage>,
    ) -> Result<IssuedCommand> {
        let accepted_at = now_utc();
        if let Some(previous) = self
            .snapshot
            .execution
            .recent_commands
            .iter()
            .find(|item| item.command_id == request.command_id)
            .cloned()
        {
            return self.idempotent_hit(command, request.command_id, accepted_at, previous);
        }
        if let Some(storage) = storage
            && let Some(previous) = storage.load_command_record(&request.command_id)?
        {
            return self.idempotent_hit(command, request.command_id, accepted_at, previous);
        }

        let accepted = CommandAccepted {
            version: PROTOCOL_VERSION.into(),
            command_id: request.command_id.clone(),
            command,
            status: CommandStatus::Accepted,
            accepted_at: accepted_at.clone(),
        };

        match command {
            CommandType::Pause => {
                let events = self.finalize_command(
                    request.command_id,
                    command,
                    accepted_at,
                    local_command_outcome("Strategy paused.", "paused"),
                );
                Ok(IssuedCommand::Immediate { accepted, events })
            }
            CommandType::Resume => {
                let events = self.finalize_command(
                    request.command_id,
                    command,
                    accepted_at,
                    local_command_outcome("Strategy resumed.", "running"),
                );
                Ok(IssuedCommand::Immediate { accepted, events })
            }
            CommandType::CancelAll
            | CommandType::FlattenNow
            | CommandType::ShutdownAfterFlatten => {
                if let Some(in_flight) = self
                    .snapshot
                    .execution
                    .pending_commands
                    .iter()
                    .find(|item| is_execution_command(item.command))
                {
                    let events = self.finalize_command(
                        request.command_id,
                        command,
                        accepted_at,
                        ExecutionOutcome::failed(format!(
                            "Execution command rejected because {} is already in flight ({})",
                            command_label(in_flight.command),
                            in_flight.command_id
                        )),
                    );
                    return Ok(IssuedCommand::Immediate { accepted, events });
                }
                let snapshot = self.snapshot.clone();
                self.snapshot
                    .execution
                    .pending_commands
                    .push(PendingCommand {
                        command_id: request.command_id.clone(),
                        command,
                        status: CommandStatus::Accepted,
                        requested_at: accepted_at,
                    });
                Ok(IssuedCommand::Deferred {
                    accepted,
                    launch: DeferredExecution {
                        command,
                        command_id: request.command_id,
                        snapshot,
                    },
                })
            }
        }
    }

    fn finish_execution(
        &mut self,
        command_id: &str,
        outcome: ExecutionOutcome,
    ) -> Vec<SequencedEngineEvent> {
        let pending = self
            .snapshot
            .execution
            .pending_commands
            .iter()
            .find(|item| item.command_id == command_id)
            .cloned();
        let Some(pending) = pending else {
            return Vec::new();
        };
        self.snapshot
            .execution
            .pending_commands
            .retain(|item| item.command_id != command_id);
        self.finalize_command(
            command_id.into(),
            pending.command,
            pending.requested_at,
            outcome,
        )
    }

    fn timeout_execution(&mut self, command_id: &str) -> Vec<SequencedEngineEvent> {
        let pending = self
            .snapshot
            .execution
            .pending_commands
            .iter()
            .find(|item| item.command_id == command_id)
            .cloned();
        let Some(pending) = pending else {
            return Vec::new();
        };
        self.snapshot
            .execution
            .pending_commands
            .retain(|item| item.command_id != command_id);
        self.finalize_command(
            command_id.into(),
            pending.command,
            pending.requested_at,
            ExecutionOutcome::timed_out("Execution timed out while waiting for terminal result."),
        )
    }

    fn finalize_command(
        &mut self,
        command_id: String,
        command: CommandType,
        accepted_at: String,
        outcome: ExecutionOutcome,
    ) -> Vec<SequencedEngineEvent> {
        self.apply_execution_outcome(&outcome);

        let ack = CommandAck {
            command_id: command_id.clone(),
            command,
            status: outcome.status,
            message: outcome.summary.clone(),
            links: outcome.links.clone(),
            emitted_at: now_utc(),
        };
        let sequence = self.next_sequence();
        self.snapshot.execution.last_command_ack = Some(command_id.clone());
        self.snapshot.execution.last_command_ack_event = Some(ack.clone());
        self.snapshot.execution.recent_commands.insert(
            0,
            CommandRecord {
                command_id,
                command,
                status: ack.status,
                summary: ack.message.clone(),
                requested_at: accepted_at.clone(),
                accepted_at: Some(accepted_at),
                finished_at: Some(ack.emitted_at.clone()),
                links: ack.links.clone(),
            },
        );
        while self.snapshot.execution.recent_commands.len() > 24 {
            self.snapshot.execution.recent_commands.pop();
        }
        self.system_events.insert(
            0,
            SystemEvent {
                level: status_level(ack.status).into(),
                source: "commands".into(),
                message: ack.message.clone(),
                created_at: ack.emitted_at.clone(),
            },
        );

        let reconcile = self.reconcile_runtime();
        let mut events = vec![SequencedEngineEvent {
            sequence,
            event: EngineEvent::CommandAck(ack),
        }];
        events.extend(self.risk_alert_events(reconcile.risk_alerts));
        if reconcile.snapshot_changed {
            events.push(self.sequenced_event(EngineEvent::RuntimeSnapshot(self.snapshot.clone())));
        }
        events
    }

    fn idempotent_hit(
        &mut self,
        command: CommandType,
        command_id: String,
        accepted_at: String,
        previous: CommandRecord,
    ) -> Result<IssuedCommand> {
        if previous.command != command {
            bail!(
                "command_id {command_id} was already used for different command: previous={:?}, requested={:?}",
                previous.command,
                command
            );
        }
        let summary = format!("Idempotent hit; previous summary: {}", previous.summary);
        self.snapshot
            .execution
            .recent_commands
            .retain(|item| item.command_id != command_id);
        self.snapshot.execution.recent_commands.insert(
            0,
            CommandRecord {
                command_id: command_id.clone(),
                command,
                status: previous.status,
                summary: summary.clone(),
                requested_at: previous.requested_at,
                accepted_at: previous.accepted_at,
                finished_at: previous.finished_at,
                links: previous.links.clone(),
            },
        );
        while self.snapshot.execution.recent_commands.len() > 24 {
            self.snapshot.execution.recent_commands.pop();
        }

        let ack = CommandAck {
            command_id: command_id.clone(),
            command,
            status: previous.status,
            message: summary.clone(),
            links: previous.links,
            emitted_at: now_utc(),
        };
        let sequence = self.next_sequence();
        self.snapshot.execution.last_command_ack = Some(command_id.clone());
        self.snapshot.execution.last_command_ack_event = Some(ack.clone());
        self.system_events.insert(
            0,
            SystemEvent {
                level: status_level(ack.status).into(),
                source: "commands".into(),
                message: ack.message.clone(),
                created_at: ack.emitted_at.clone(),
            },
        );

        let accepted = CommandAccepted {
            version: PROTOCOL_VERSION.into(),
            command_id,
            command,
            status: CommandStatus::Accepted,
            accepted_at,
        };
        Ok(IssuedCommand::Immediate {
            accepted,
            events: vec![SequencedEngineEvent {
                sequence,
                event: EngineEvent::CommandAck(ack),
            }],
        })
    }

    fn reconcile_runtime(&mut self) -> ReconcileOutcome {
        let previous_snapshot = self.snapshot.clone();

        let risk_evaluation = risk::evaluate(
            &self.snapshot.runtime,
            &self.snapshot.risk,
            &self.snapshot.strategy.config,
        );
        self.snapshot.risk = risk_evaluation.state;
        for event in &risk_evaluation.new_events {
            self.record_risk_event(event.clone());
        }

        self.snapshot.strategy = strategy::reconcile(
            &self.snapshot.runtime,
            &self.snapshot.risk,
            &self.snapshot.strategy,
        );
        self.snapshot.risk.unacked_alerts = self
            .risk_events
            .iter()
            .filter(|event| event.acknowledged_at.is_none())
            .count() as u32;

        ReconcileOutcome {
            snapshot_changed: self.snapshot != previous_snapshot,
            risk_alerts: risk_evaluation.new_events,
        }
    }

    fn record_risk_event(&mut self, event: RiskEvent) {
        self.risk_events.insert(0, event.clone());
        while self.risk_events.len() > 50 {
            self.risk_events.pop();
        }
        self.system_events.insert(
            0,
            SystemEvent {
                level: match event.severity {
                    crate::protocol::RiskLevel::Ok | crate::protocol::RiskLevel::Watch => "info",
                    crate::protocol::RiskLevel::Warning => "warn",
                    crate::protocol::RiskLevel::Danger => "error",
                }
                .into(),
                source: "risk".into(),
                message: format!("{}: {}", event.code, event.message),
                created_at: event.created_at.clone(),
            },
        );
        while self.system_events.len() > 50 {
            self.system_events.pop();
        }
    }

    fn risk_alert_events(&mut self, alerts: Vec<RiskEvent>) -> Vec<SequencedEngineEvent> {
        alerts
            .into_iter()
            .map(|alert| self.sequenced_event(EngineEvent::RiskAlert(alert)))
            .collect()
    }

    fn apply_execution_outcome(&mut self, outcome: &ExecutionOutcome) {
        if let Some(open_orders) = &outcome.open_orders {
            self.snapshot.execution.open_orders = open_orders.clone();
        }
        if let Some(recent_fills) = &outcome.recent_fills {
            self.snapshot.execution.recent_fills = recent_fills.clone();
        }
        apply_execution_runtime_patch(&mut self.snapshot, &outcome.runtime_patch);
    }

    fn apply_execution_patch(&mut self, patch: &ExecutionStatePatch) -> bool {
        if patch.is_noop() {
            return false;
        }

        let mut changed = false;

        if let Some(open_orders) = &patch.open_orders
            && self.snapshot.execution.open_orders != *open_orders
        {
            self.snapshot.execution.open_orders = open_orders.clone();
            changed = true;
        }

        if !patch.recent_fills.is_empty() {
            for fill in patch.recent_fills.iter().rev() {
                self.snapshot.execution.recent_fills.insert(0, fill.clone());
            }
            changed = true;
        }

        changed |= apply_execution_runtime_patch(&mut self.snapshot, &patch.runtime_patch);
        changed
    }

    fn next_sequence(&mut self) -> u64 {
        self.last_sequence += 1;
        self.last_sequence
    }

    fn sequenced_event(&mut self, event: EngineEvent) -> SequencedEngineEvent {
        SequencedEngineEvent {
            sequence: self.next_sequence(),
            event,
        }
    }
}

fn persist_runtime_state(
    storage: Option<&SqliteStorage>,
    aggregate: &RuntimeAggregate,
) -> Result<()> {
    let Some(storage) = storage else {
        return Ok(());
    };

    storage.persist_runtime(&PersistedRuntime {
        snapshot: aggregate.snapshot.clone(),
        risk_events: aggregate.risk_events.clone(),
        system_events: aggregate.system_events.clone(),
        last_sequence: aggregate.last_sequence,
    })
}

fn apply_execution_runtime_patch(
    snapshot: &mut RuntimeSnapshot,
    runtime_patch: &ExecutionRuntimePatch,
) -> bool {
    let mut changed = false;

    if let Some(strategy_state) = &runtime_patch.strategy_state
        && snapshot.runtime.strategy_state != *strategy_state
    {
        snapshot.runtime.strategy_state = strategy_state.clone();
        changed = true;
    }
    if let Some(position_qty) = runtime_patch.position_qty
        && (snapshot.runtime.position_qty - position_qty).abs() > f64::EPSILON
    {
        snapshot.runtime.position_qty = position_qty;
        changed = true;
    }
    if let Some(position_avg_price) = runtime_patch.position_avg_price
        && (snapshot.runtime.position_avg_price - position_avg_price).abs() > f64::EPSILON
    {
        snapshot.runtime.position_avg_price = position_avg_price;
        changed = true;
    }
    if let Some(unrealized_pnl) = runtime_patch.unrealized_pnl
        && (snapshot.runtime.unrealized_pnl - unrealized_pnl).abs() > f64::EPSILON
    {
        snapshot.runtime.unrealized_pnl = unrealized_pnl;
        changed = true;
    }
    if let Some(realized_pnl) = runtime_patch.realized_pnl
        && (snapshot.runtime.realized_pnl - realized_pnl).abs() > f64::EPSILON
    {
        snapshot.runtime.realized_pnl = realized_pnl;
        changed = true;
    }

    changed
}

impl From<&RuntimeAggregate> for ReadModel {
    fn from(value: &RuntimeAggregate) -> Self {
        Self {
            snapshot: value.snapshot.clone(),
            risk_events: value.risk_events.clone(),
            system_events: value.system_events.clone(),
            last_sequence: value.last_sequence,
        }
    }
}

pub(crate) fn now_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn round_price(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn local_command_outcome(summary: &str, strategy_state: &str) -> ExecutionOutcome {
    let mut outcome = ExecutionOutcome::completed(summary);
    outcome.runtime_patch = ExecutionRuntimePatch {
        strategy_state: Some(strategy_state.into()),
        ..ExecutionRuntimePatch::default()
    };
    outcome.links = CommandLinks::default();
    outcome
}

fn is_execution_command(command: CommandType) -> bool {
    matches!(
        command,
        CommandType::CancelAll | CommandType::FlattenNow | CommandType::ShutdownAfterFlatten
    )
}

fn command_label(command: CommandType) -> &'static str {
    match command {
        CommandType::Pause => "pause",
        CommandType::Resume => "resume",
        CommandType::CancelAll => "cancel-all",
        CommandType::FlattenNow => "flatten-now",
        CommandType::ShutdownAfterFlatten => "shutdown-after-flatten",
    }
}

fn status_level(status: CommandStatus) -> &'static str {
    match status {
        CommandStatus::Pending | CommandStatus::Accepted | CommandStatus::Completed => "info",
        CommandStatus::TimedOut => "warn",
        CommandStatus::Failed => "error",
    }
}
