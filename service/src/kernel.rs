use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::background::spawn_task;
use crate::protocol::{
    CommandAccepted, CommandAck, CommandRecord, CommandRequest, CommandStatus, CommandType,
    ConnectionState, PROTOCOL_VERSION, PendingCommand, PriceUpdated, RecentFill, RiskEvent,
    RuntimeSnapshot, SystemEvent,
};
use crate::storage::{PersistedRuntime, SqliteStorage};

const ENGINE_COMMAND_BUFFER: usize = 256;
const ENGINE_EVENT_BUFFER: usize = 256;

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
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    CommandAck(CommandAck),
    PriceUpdated(PriceUpdated),
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
    spawn_engine_with_runtime(PersistedRuntime::in_memory_bootstrap(), None)
}

pub fn spawn_engine_with_runtime(
    runtime: PersistedRuntime,
    storage: Option<SqliteStorage>,
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
        commands_rx,
        events_tx,
        read_model.clone(),
        aggregate,
        storage,
    ));

    (EngineHandle { commands_tx }, read_model, events_rx)
}

async fn run_engine(
    mut commands_rx: mpsc::Receiver<EngineCommand>,
    events_tx: mpsc::Sender<SequencedEngineEvent>,
    read_model: SharedReadModel,
    mut aggregate: RuntimeAggregate,
    storage: Option<SqliteStorage>,
) {
    while let Some(command) = commands_rx.recv().await {
        match command {
            EngineCommand::SubmitCommand {
                command,
                request,
                reply_to,
            } => {
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let (accepted, ack) = aggregate.issue_command(command, request);
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
                let _ = reply_to.send(Ok(accepted));
                if events_tx.send(ack).await.is_err() {
                    warn!("engine event channel closed while publishing command ack");
                }
            }
            EngineCommand::EmitPriceTick { reply_to } => {
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let (tick, event) = aggregate.emit_price_tick();
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
                if events_tx.send(event).await.is_err() {
                    warn!("engine event channel closed while publishing price update");
                }
            }
            EngineCommand::SyncConnection {
                connection,
                reply_to,
            } => {
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let event = aggregate.sync_connection(connection);
                if event.is_some()
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
                if let Some(event) = event
                    && events_tx.send(event).await.is_err()
                {
                    warn!("engine event channel closed while publishing connection change");
                }
            }
            EngineCommand::SyncRuntime { patch, reply_to } => {
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let event = aggregate.sync_runtime(patch);
                if event.is_some()
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
                if let Some(event) = event
                    && events_tx.send(event).await.is_err()
                {
                    warn!("engine event channel closed while publishing runtime snapshot");
                }
            }
            EngineCommand::SyncMarketPrices {
                last_price,
                mark_price,
                emitted_at,
                reply_to,
            } => {
                let previous = storage.as_ref().map(|_| aggregate.clone());
                let event = aggregate.sync_market_prices(last_price, mark_price, emitted_at);
                if event.is_some()
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
                if let Some(event) = event
                    && events_tx.send(event).await.is_err()
                {
                    warn!("engine event channel closed while publishing market prices");
                }
            }
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

impl RuntimeAggregate {
    fn from_persisted(runtime: PersistedRuntime) -> Self {
        Self {
            snapshot: runtime.snapshot,
            risk_events: runtime.risk_events,
            system_events: runtime.system_events,
            last_sequence: runtime.last_sequence,
        }
    }

    fn emit_price_tick(&mut self) -> (PriceUpdated, SequencedEngineEvent) {
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
        let sequence = self.next_sequence();
        (
            tick.clone(),
            SequencedEngineEvent {
                sequence,
                event: EngineEvent::PriceUpdated(tick),
            },
        )
    }

    fn sync_connection(&mut self, connection: ConnectionState) -> Option<SequencedEngineEvent> {
        if self.snapshot.connection == connection {
            return None;
        }

        self.snapshot.connection = connection.clone();
        let sequence = self.next_sequence();
        Some(SequencedEngineEvent {
            sequence,
            event: EngineEvent::ConnectionChanged(connection),
        })
    }

    fn sync_runtime(&mut self, patch: RuntimePatch) -> Option<SequencedEngineEvent> {
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
            return None;
        }

        let sequence = self.next_sequence();
        Some(SequencedEngineEvent {
            sequence,
            event: EngineEvent::RuntimeSnapshot(self.snapshot.clone()),
        })
    }

    fn sync_market_prices(
        &mut self,
        last_price: Option<f64>,
        mark_price: Option<f64>,
        emitted_at: String,
    ) -> Option<SequencedEngineEvent> {
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
            return None;
        }

        let tick = PriceUpdated {
            symbol: self.snapshot.runtime.symbol.clone(),
            last_price: self.snapshot.runtime.last_price,
            mark_price: self.snapshot.runtime.mark_price,
            emitted_at,
        };
        let sequence = self.next_sequence();
        Some(SequencedEngineEvent {
            sequence,
            event: EngineEvent::PriceUpdated(tick),
        })
    }

    fn issue_command(
        &mut self,
        command: CommandType,
        request: CommandRequest,
    ) -> (CommandAccepted, SequencedEngineEvent) {
        let accepted_at = now_utc();
        self.snapshot
            .execution
            .pending_commands
            .push(PendingCommand {
                command_id: request.command_id.clone(),
                command,
                status: CommandStatus::Accepted,
                requested_at: accepted_at.clone(),
            });

        let message = self.apply_command(command);
        let ack = CommandAck {
            command_id: request.command_id.clone(),
            command,
            status: CommandStatus::Completed,
            message: message.clone(),
            emitted_at: now_utc(),
        };
        let sequence = self.next_sequence();

        self.snapshot
            .execution
            .pending_commands
            .retain(|item| item.command_id != request.command_id);
        self.snapshot.execution.last_command_ack = Some(request.command_id.clone());
        self.snapshot.execution.last_command_ack_event = Some(ack.clone());
        self.snapshot.execution.recent_commands.insert(
            0,
            CommandRecord {
                command_id: request.command_id.clone(),
                command,
                status: ack.status,
                summary: ack.message.clone(),
                requested_at: accepted_at.clone(),
                accepted_at: Some(accepted_at.clone()),
                finished_at: Some(ack.emitted_at.clone()),
            },
        );
        while self.snapshot.execution.recent_commands.len() > 24 {
            self.snapshot.execution.recent_commands.pop();
        }
        self.system_events.insert(
            0,
            SystemEvent {
                level: "info".into(),
                source: "commands".into(),
                message,
                created_at: ack.emitted_at.clone(),
            },
        );

        let accepted = CommandAccepted {
            version: PROTOCOL_VERSION.into(),
            command_id: request.command_id,
            command,
            status: CommandStatus::Accepted,
            accepted_at,
        };
        (
            accepted,
            SequencedEngineEvent {
                sequence,
                event: EngineEvent::CommandAck(ack),
            },
        )
    }

    fn apply_command(&mut self, command: CommandType) -> String {
        match command {
            CommandType::Pause => {
                self.snapshot.runtime.strategy_state = "paused".into();
                "Strategy paused.".into()
            }
            CommandType::Resume => {
                self.snapshot.runtime.strategy_state = "running".into();
                "Strategy resumed.".into()
            }
            CommandType::CancelAll => {
                self.snapshot.execution.open_orders.clear();
                "All open orders cancelled.".into()
            }
            CommandType::FlattenNow => {
                self.snapshot.runtime.position_qty = 0.0;
                self.snapshot.runtime.unrealized_pnl = 0.0;
                "Position flattened.".into()
            }
            CommandType::ShutdownAfterFlatten => {
                self.snapshot.runtime.position_qty = 0.0;
                self.snapshot.runtime.strategy_state = "paused".into();
                "Position flattened and shutdown requested.".into()
            }
        }
    }

    fn next_sequence(&mut self) -> u64 {
        self.last_sequence += 1;
        self.last_sequence
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
