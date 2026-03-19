use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::protocol::{
    CommandAccepted, CommandAck, CommandRecord, CommandRequest, CommandStatus, CommandType,
    PROTOCOL_VERSION, PendingCommand, PriceUpdated, RecentFill, RiskEvent, RiskLevel,
    RuntimeSnapshot, SystemEvent,
};

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
        reply_to: oneshot::Sender<CommandAccepted>,
    },
    EmitPriceTick {
        reply_to: oneshot::Sender<PriceUpdated>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    CommandAck(CommandAck),
    PriceUpdated(PriceUpdated),
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
        reply_rx
            .await
            .context("engine loop dropped before acknowledging command")
    }

    pub async fn emit_price_tick(&self) -> Result<PriceUpdated> {
        let (reply_to, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(EngineCommand::EmitPriceTick { reply_to })
            .await
            .context("failed to enqueue price tick")?;
        reply_rx
            .await
            .context("engine loop dropped before emitting price tick")
    }
}

pub fn spawn_engine() -> (
    EngineHandle,
    SharedReadModel,
    mpsc::Receiver<SequencedEngineEvent>,
) {
    let aggregate = RuntimeAggregate::new();
    let read_model = Arc::new(RwLock::new(ReadModel::from(&aggregate)));
    let (commands_tx, commands_rx) = mpsc::channel(ENGINE_COMMAND_BUFFER);
    let (events_tx, events_rx) = mpsc::channel(ENGINE_EVENT_BUFFER);

    tokio::spawn(run_engine(
        commands_rx,
        events_tx,
        read_model.clone(),
        aggregate,
    ));

    (EngineHandle { commands_tx }, read_model, events_rx)
}

async fn run_engine(
    mut commands_rx: mpsc::Receiver<EngineCommand>,
    events_tx: mpsc::Sender<SequencedEngineEvent>,
    read_model: SharedReadModel,
    mut aggregate: RuntimeAggregate,
) {
    while let Some(command) = commands_rx.recv().await {
        match command {
            EngineCommand::SubmitCommand {
                command,
                request,
                reply_to,
            } => {
                let (accepted, ack) = aggregate.issue_command(command, request);
                replace_read_model(&read_model, &aggregate);
                let _ = reply_to.send(accepted);
                if events_tx.send(ack).await.is_err() {
                    warn!("engine event channel closed while publishing command ack");
                }
            }
            EngineCommand::EmitPriceTick { reply_to } => {
                let (tick, event) = aggregate.emit_price_tick();
                replace_read_model(&read_model, &aggregate);
                let _ = reply_to.send(tick.clone());
                if events_tx.send(event).await.is_err() {
                    warn!("engine event channel closed while publishing price update");
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
    fn new() -> Self {
        let now = now_utc();
        Self {
            snapshot: RuntimeSnapshot::sample(),
            risk_events: vec![RiskEvent {
                severity: RiskLevel::Watch,
                code: "MARGIN_USAGE_WATCH".into(),
                message: "Margin usage reached 39% of configured threshold.".into(),
                created_at: now.clone(),
                acknowledged_at: None,
            }],
            system_events: vec![SystemEvent {
                level: "info".into(),
                source: "bootstrap".into(),
                message: "Rust in-memory runtime bootstrapped.".into(),
                created_at: now,
            }],
            last_sequence: 0,
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
