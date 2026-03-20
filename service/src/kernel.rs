use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::background::spawn_task;
use crate::execution::{
    ExecutionAdapter, ExecutionOutcome, ExecutionRuntimePatch, FakeExecutionAdapter,
};
use crate::protocol::{
    CommandAccepted, CommandAck, CommandLinks, CommandRecord, CommandRequest, CommandStatus,
    CommandType, ConnectionState, PROTOCOL_VERSION, PendingCommand, PriceUpdated, RecentFill,
    RiskEvent, RuntimeSnapshot, SystemEvent,
};
use crate::{risk, strategy};
use crate::storage::{PersistedRuntime, SqliteStorage};

const ENGINE_COMMAND_BUFFER: usize = 256;
const ENGINE_EVENT_BUFFER: usize = 256;
const EXECUTION_TIMEOUT: Duration = Duration::from_millis(250);

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

fn spawn_engine_with_runtime_and_adapter(
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
    while let Some(command) = commands_rx.recv().await {
        match command {
            EngineCommand::SubmitCommand {
                command,
                request,
                reply_to,
            } => {
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let issued = match aggregate.issue_command(command, request, storage.as_ref()) {
                    Ok(value) => value,
                    Err(error) => {
                        if let Some(previous) = previous {
                            aggregate = previous;
                        }
                        let _ = reply_to.send(Err(error));
                        continue;
                    }
                };
                if let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate) {
                    if let Some(previous) = previous {
                        aggregate = previous;
                    }
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
                    IssuedCommand::Immediate { accepted, events } => {
                        let _ = reply_to.send(Ok(accepted));
                        publish_events(&events_tx, events).await;
                    }
                    IssuedCommand::Deferred { accepted, launch } => {
                        let execution_adapter = execution_adapter.clone();
                        let commands_tx = commands_tx.clone();
                        let timeout_tx = commands_tx.clone();
                        let timeout_command_id = launch.command_id.clone();
                        spawn_task(async move {
                            let outcome = match execution_adapter
                                .execute(launch.command, &launch.command_id, &launch.snapshot)
                                .await
                            {
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
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let (tick, events) = aggregate.emit_price_tick();
                if let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate) {
                    if let Some(previous) = previous {
                        aggregate = previous;
                    }
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
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let events = aggregate.sync_connection(connection);
                if !events.is_empty()
                    && let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate)
                {
                    if let Some(previous) = previous {
                        aggregate = previous;
                    }
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
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let events = aggregate.sync_runtime(patch);
                if !events.is_empty()
                    && let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate)
                {
                    if let Some(previous) = previous {
                        aggregate = previous;
                    }
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
            EngineCommand::SyncMarketPrices {
                last_price,
                mark_price,
                emitted_at,
                reply_to,
            } => {
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let events = aggregate.sync_market_prices(last_price, mark_price, emitted_at);
                if !events.is_empty()
                    && let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate)
                {
                    if let Some(previous) = previous {
                        aggregate = previous;
                    }
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
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let events = aggregate.finish_execution(&command_id, outcome);
                if events.is_empty() {
                    continue;
                }
                if let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate) {
                    if let Some(previous) = previous {
                        aggregate = previous;
                    }
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
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let events = aggregate.timeout_execution(&command_id);
                if events.is_empty() {
                    continue;
                }
                if let Err(error) = persist_runtime_state(storage.as_ref(), &aggregate) {
                    if let Some(previous) = previous {
                        aggregate = previous;
                    }
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
        let timestamp = now_utc();
        let _ = aggregate.reconcile_runtime(timestamp, false);
        aggregate
    }

    fn emit_price_tick(&mut self) -> (PriceUpdated, Vec<SequencedEngineEvent>) {
        let emitted_at = now_utc();
        self.snapshot.runtime.last_price = round_price(self.snapshot.runtime.last_price + 0.11);
        self.snapshot.runtime.mark_price = round_price(self.snapshot.runtime.mark_price + 0.08);
        self.snapshot.connection.last_heartbeat_at = emitted_at.clone();
        self.snapshot.connection.stale_age_ms = 0;
        let tick = PriceUpdated {
            symbol: self.snapshot.runtime.symbol.clone(),
            last_price: self.snapshot.runtime.last_price,
            mark_price: self.snapshot.runtime.mark_price,
            emitted_at,
        };
        let reconcile = self.reconcile_runtime(tick.emitted_at.clone(), true);
        let mut events = vec![self.sequenced_event(EngineEvent::PriceUpdated(tick.clone()))];
        events.extend(self.risk_alert_events(reconcile.risk_alerts));
        if reconcile.snapshot_changed {
            events.push(self.sequenced_event(EngineEvent::RuntimeSnapshot(
                self.snapshot.clone(),
            )));
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

        let reconcile = self.reconcile_runtime(now_utc(), true);
        let mut events = vec![self.sequenced_event(EngineEvent::RuntimeSnapshot(
            self.snapshot.clone(),
        ))];
        events.extend(self.risk_alert_events(reconcile.risk_alerts));
        events
    }

    fn sync_market_prices(
        &mut self,
        last_price: Option<f64>,
        mark_price: Option<f64>,
        emitted_at: String,
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

        if !changed {
            return Vec::new();
        }

        let tick = PriceUpdated {
            symbol: self.snapshot.runtime.symbol.clone(),
            last_price: self.snapshot.runtime.last_price,
            mark_price: self.snapshot.runtime.mark_price,
            emitted_at,
        };
        let reconcile = self.reconcile_runtime(tick.emitted_at.clone(), true);
        let mut events = vec![self.sequenced_event(EngineEvent::PriceUpdated(tick))];
        events.extend(self.risk_alert_events(reconcile.risk_alerts));
        if reconcile.snapshot_changed {
            events.push(self.sequenced_event(EngineEvent::RuntimeSnapshot(
                self.snapshot.clone(),
            )));
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

        let reconcile = self.reconcile_runtime(
            now_utc(),
            outcome.open_orders.is_none() && runtime_patch_has_changes(&outcome.runtime_patch),
        );
        let mut events = vec![SequencedEngineEvent {
            sequence,
            event: EngineEvent::CommandAck(ack),
        }];
        events.extend(self.risk_alert_events(reconcile.risk_alerts));
        if reconcile.snapshot_changed {
            events.push(self.sequenced_event(EngineEvent::RuntimeSnapshot(
                self.snapshot.clone(),
            )));
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

    fn reconcile_runtime(&mut self, timestamp: String, sync_execution_orders: bool) -> ReconcileOutcome {
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

        self.snapshot.strategy =
            strategy::reconcile(&self.snapshot.runtime, &self.snapshot.risk, &self.snapshot.strategy);
        if sync_execution_orders {
            let desired_orders = strategy::desired_open_orders(
                &self.snapshot.strategy,
                &self.snapshot.runtime.strategy_state,
                self.snapshot.risk.breaker_engaged,
                &self.snapshot.execution.open_orders,
                &timestamp,
            );
            if self.snapshot.execution.open_orders != desired_orders {
                self.snapshot.execution.open_orders = desired_orders;
            }
        }
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
        if let Some(strategy_state) = &outcome.runtime_patch.strategy_state {
            self.snapshot.runtime.strategy_state = strategy_state.clone();
        }
        if let Some(position_qty) = outcome.runtime_patch.position_qty {
            self.snapshot.runtime.position_qty = position_qty;
        }
        if let Some(position_avg_price) = outcome.runtime_patch.position_avg_price {
            self.snapshot.runtime.position_avg_price = position_avg_price;
        }
        if let Some(unrealized_pnl) = outcome.runtime_patch.unrealized_pnl {
            self.snapshot.runtime.unrealized_pnl = unrealized_pnl;
        }
        if let Some(realized_pnl) = outcome.runtime_patch.realized_pnl {
            self.snapshot.runtime.realized_pnl = realized_pnl;
        }
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

fn runtime_patch_has_changes(patch: &ExecutionRuntimePatch) -> bool {
    patch.strategy_state.is_some()
        || patch.position_qty.is_some()
        || patch.position_avg_price.is_some()
        || patch.unrealized_pnl.is_some()
        || patch.realized_pnl.is_some()
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
