use std::collections::VecDeque;

use crate::protocol::{
    CommandAck, CommandLinks, CommandRecord, CommandStatus, CommandType, PendingCommand,
    RecentFill, RiskEvent, RiskLevel, RuntimeSnapshot, StrategyState, SystemEvent,
};

pub const COMMAND_TIMEOUT_TICKS: u64 = 15;
const COMMAND_TIMELINE_LIMIT: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Dashboard,
    Grid,
    Market,
    Events,
    Help,
}

impl Page {
    pub fn panel_count(self) -> usize {
        match self {
            Self::Dashboard => 6,
            Self::Grid => 3,
            Self::Market => 3,
            Self::Events => 4,
            Self::Help => 2,
        }
    }

    pub fn normalize_focus(self, focus_index: usize) -> usize {
        let panel_count = self.panel_count();
        if panel_count == 0 {
            0
        } else {
            focus_index % panel_count
        }
    }

    pub fn focus_label(self, focus_index: usize) -> &'static str {
        const DASHBOARD: [&str; 6] = [
            "Execution Focus",
            "Open Orders",
            "Recent Fills",
            "Risk + Alerts",
            "Market + Health",
            "Command Timeline",
        ];
        const GRID: [&str; 3] = ["Active Grid Levels", "Grid Summary", "Operator Notes"];
        const MARKET: [&str; 3] = ["Tape", "Connectivity", "Runtime"];
        const EVENTS: [&str; 4] = ["Fills", "Alerts", "Commands", "System"];
        const HELP: [&str; 2] = ["Keymap", "Ops Notes"];

        let labels = match self {
            Self::Dashboard => &DASHBOARD[..],
            Self::Grid => &GRID[..],
            Self::Market => &MARKET[..],
            Self::Events => &EVENTS[..],
            Self::Help => &HELP[..],
        };
        labels[self.normalize_focus(focus_index)]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    Confirm(CommandType),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotBootstrapState {
    WaitingFirstSnapshot,
    SnapshotRetrying {
        last_error: String,
        retry_count: u32,
        retry_in_ms: u64,
    },
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Warning,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub level: ToastLevel,
    pub message: String,
    pub ttl_ticks: u16,
}

#[derive(Debug, Clone, Default)]
pub struct DirtyFlags {
    pub connection: bool,
    pub runtime: bool,
    pub execution: bool,
    pub risk: bool,
    pub ui: bool,
}

impl DirtyFlags {
    pub fn all() -> Self {
        Self {
            connection: true,
            runtime: true,
            execution: true,
            risk: true,
            ui: true,
        }
    }

    pub fn any(&self) -> bool {
        self.connection || self.runtime || self.execution || self.risk || self.ui
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionViewState {
    pub http_available: bool,
    pub market_ws_connected: bool,
    pub user_stream_connected: Option<bool>,
    pub latency_ms: Option<u32>,
    pub last_heartbeat_at: String,
    pub market_reconnect_backoff_ms: u64,
    pub stale_age_ms: u64,
    pub ws_connected: bool,
    pub reconnect_attempt: u32,
    pub reconnect_backoff_ms: u64,
}

#[derive(Debug, Clone)]
pub struct RuntimeViewState {
    pub symbol: String,
    pub env: String,
    pub session_state: String,
    pub strategy_state: String,
    pub last_price: f64,
    pub mark_price: f64,
    pub position_qty: f64,
    pub position_avg_price: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandTimelineStage {
    Pending,
    Accepted,
    Ack,
    Failed,
    TimedOut,
}

impl CommandTimelineStage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Accepted => "ACCEPTED",
            Self::Ack => "ACK",
            Self::Failed => "FAILED",
            Self::TimedOut => "TIMED OUT",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ack | Self::Failed | Self::TimedOut)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTimelineEntry {
    pub command_id: String,
    pub command: CommandType,
    pub stage: CommandTimelineStage,
    pub summary: String,
    pub requested_at: String,
    pub accepted_at: Option<String>,
    pub finished_at: Option<String>,
    pub links: CommandLinks,
    pub timeout_at_tick: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ExecutionViewState {
    pub open_orders: Vec<crate::protocol::OpenOrder>,
    pub recent_fills: VecDeque<RecentFill>,
    pub pending_commands: Vec<PendingCommand>,
    pub last_command_ack: Option<CommandAck>,
    pub command_timeline: VecDeque<CommandTimelineEntry>,
}

#[derive(Debug, Clone)]
pub struct RiskViewState {
    pub current_notional: f64,
    pub max_notional: f64,
    pub daily_loss_limit: f64,
    pub stop_loss_pct: f64,
    pub risk_level: RiskLevel,
    pub breaker_engaged: bool,
    pub unacked_alerts: u32,
    pub alerts: VecDeque<RiskEvent>,
}

#[derive(Debug, Clone)]
pub struct UiState {
    pub page: Page,
    pub focus_index: usize,
    pub modal: Option<Modal>,
    pub toast: Option<Toast>,
    pub should_quit: bool,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub connection: ConnectionViewState,
    pub runtime: RuntimeViewState,
    pub execution: ExecutionViewState,
    pub risk: RiskViewState,
    pub strategy: StrategyState,
    pub ui: UiState,
    pub snapshot_state: SnapshotBootstrapState,
    pub system_events: VecDeque<SystemEvent>,
    pub dirty: DirtyFlags,
    pub immediate_render: bool,
    pub clock_ticks: u64,
    next_command_seq: u64,
}

impl AppState {
    pub fn waiting_first_snapshot() -> Self {
        Self {
            connection: ConnectionViewState {
                http_available: false,
                market_ws_connected: false,
                user_stream_connected: None,
                latency_ms: None,
                last_heartbeat_at: String::new(),
                market_reconnect_backoff_ms: 0,
                stale_age_ms: 0,
                ws_connected: false,
                reconnect_attempt: 0,
                reconnect_backoff_ms: 0,
            },
            runtime: RuntimeViewState {
                symbol: String::new(),
                env: String::new(),
                session_state: String::new(),
                strategy_state: String::new(),
                last_price: 0.0,
                mark_price: 0.0,
                position_qty: 0.0,
                position_avg_price: 0.0,
                unrealized_pnl: 0.0,
                realized_pnl: 0.0,
            },
            execution: ExecutionViewState {
                open_orders: Vec::new(),
                recent_fills: VecDeque::new(),
                pending_commands: Vec::new(),
                last_command_ack: None,
                command_timeline: VecDeque::new(),
            },
            risk: RiskViewState {
                current_notional: 0.0,
                max_notional: 0.0,
                daily_loss_limit: 0.0,
                stop_loss_pct: 0.0,
                risk_level: RiskLevel::Ok,
                breaker_engaged: false,
                unacked_alerts: 0,
                alerts: VecDeque::new(),
            },
            strategy: StrategyState::default(),
            ui: UiState {
                page: Page::Dashboard,
                focus_index: 0,
                modal: None,
                toast: None,
                should_quit: false,
                width: 160,
                height: 44,
            },
            snapshot_state: SnapshotBootstrapState::WaitingFirstSnapshot,
            system_events: VecDeque::new(),
            dirty: DirtyFlags::all(),
            immediate_render: true,
            clock_ticks: 0,
            next_command_seq: 1,
        }
    }

    pub fn sample() -> Self {
        let mut state = Self::from_snapshot(RuntimeSnapshot::sample());
        state.connection.ws_connected = true;
        state.risk.alerts.push_front(RiskEvent {
            severity: RiskLevel::Watch,
            code: "MARGIN_USAGE_WATCH".into(),
            message: "Margin usage reached 39% of configured threshold.".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            acknowledged_at: None,
        });
        state.system_events.push_front(SystemEvent {
            level: "info".into(),
            source: "bootstrap".into(),
            message: "Initial runtime snapshot loaded.".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
        });
        state.system_events.push_front(SystemEvent {
            level: "info".into(),
            source: "transport".into(),
            message: "WebSocket connected and streaming.".into(),
            created_at: "2025-01-01T00:00:02Z".into(),
        });
        state
            .execution
            .command_timeline
            .push_front(CommandTimelineEntry {
                command_id: "cmd_hist_0002".into(),
                command: CommandType::FlattenNow,
                stage: CommandTimelineStage::Failed,
                summary: "Exchange rejected flatten request due to reduce-only mismatch.".into(),
                requested_at: "2025-01-01T00:00:09Z".into(),
                accepted_at: Some("2025-01-01T00:00:10Z".into()),
                finished_at: Some("2025-01-01T00:00:13Z".into()),
                links: CommandLinks::default(),
                timeout_at_tick: None,
            });
        state
            .execution
            .command_timeline
            .push_front(CommandTimelineEntry {
                command_id: "cmd_hist_0001".into(),
                command: CommandType::Pause,
                stage: CommandTimelineStage::Ack,
                summary: "Strategy acknowledged pause and stopped placing new orders.".into(),
                requested_at: "2025-01-01T00:00:03Z".into(),
                accepted_at: Some("2025-01-01T00:00:04Z".into()),
                finished_at: Some("2025-01-01T00:00:05Z".into()),
                links: CommandLinks::default(),
                timeout_at_tick: None,
            });
        state.trim_command_timeline();
        state
    }

    pub fn from_snapshot(snapshot: RuntimeSnapshot) -> Self {
        let command_timeline = command_timeline_from_snapshot(&snapshot);
        Self {
            connection: ConnectionViewState {
                http_available: snapshot.connection.http_available,
                market_ws_connected: snapshot.connection.ws_connected,
                user_stream_connected: snapshot.connection.user_stream_connected,
                latency_ms: snapshot.connection.latency_ms,
                last_heartbeat_at: snapshot.connection.last_heartbeat_at,
                market_reconnect_backoff_ms: snapshot.connection.reconnect_backoff_ms,
                stale_age_ms: snapshot.connection.stale_age_ms,
                ws_connected: false,
                reconnect_attempt: 0,
                reconnect_backoff_ms: 0,
            },
            runtime: RuntimeViewState {
                symbol: snapshot.runtime.symbol,
                env: snapshot.runtime.env,
                session_state: snapshot.runtime.session_state,
                strategy_state: snapshot.runtime.strategy_state,
                last_price: snapshot.runtime.last_price,
                mark_price: snapshot.runtime.mark_price,
                position_qty: snapshot.runtime.position_qty,
                position_avg_price: snapshot.runtime.position_avg_price,
                unrealized_pnl: snapshot.runtime.unrealized_pnl,
                realized_pnl: snapshot.runtime.realized_pnl,
            },
            execution: ExecutionViewState {
                open_orders: snapshot.execution.open_orders,
                recent_fills: VecDeque::from(snapshot.execution.recent_fills),
                pending_commands: snapshot.execution.pending_commands,
                last_command_ack: snapshot.execution.last_command_ack_event,
                command_timeline,
            },
            risk: RiskViewState {
                current_notional: snapshot.risk.current_notional,
                max_notional: snapshot.risk.max_notional,
                daily_loss_limit: snapshot.risk.daily_loss_limit,
                stop_loss_pct: snapshot.risk.stop_loss_pct,
                risk_level: snapshot.risk.risk_level,
                breaker_engaged: snapshot.risk.breaker_engaged,
                unacked_alerts: snapshot.risk.unacked_alerts,
                alerts: VecDeque::new(),
            },
            strategy: snapshot.strategy,
            ui: UiState {
                page: Page::Dashboard,
                focus_index: 0,
                modal: None,
                toast: None,
                should_quit: false,
                width: 160,
                height: 44,
            },
            snapshot_state: SnapshotBootstrapState::Ready,
            system_events: VecDeque::new(),
            dirty: DirtyFlags::all(),
            immediate_render: true,
            clock_ticks: 0,
            next_command_seq: 1,
        }
    }

    pub fn queue_command(&mut self, command: CommandType) -> String {
        let command_id = format!("local_cmd_{:04}", self.next_command_seq);
        self.next_command_seq += 1;
        self.execution.pending_commands.push(PendingCommand {
            command_id: command_id.clone(),
            command,
            status: CommandStatus::Pending,
            requested_at: self.local_timestamp(),
        });
        self.execution
            .command_timeline
            .push_front(CommandTimelineEntry {
                command_id: command_id.clone(),
                command,
                stage: CommandTimelineStage::Pending,
                summary: "Waiting for service acceptance.".into(),
                requested_at: self.local_timestamp(),
                accepted_at: None,
                finished_at: None,
                links: CommandLinks::default(),
                timeout_at_tick: Some(self.clock_ticks + COMMAND_TIMEOUT_TICKS),
            });
        self.trim_command_timeline();
        command_id
    }

    pub fn mark_dirty(&mut self, flags: DirtyFlags, immediate: bool) {
        self.dirty.connection |= flags.connection;
        self.dirty.runtime |= flags.runtime;
        self.dirty.execution |= flags.execution;
        self.dirty.risk |= flags.risk;
        self.dirty.ui |= flags.ui;
        self.immediate_render |= immediate;
    }

    pub fn take_immediate_render(&mut self) -> bool {
        let value = self.immediate_render;
        self.immediate_render = false;
        value
    }

    pub fn local_timestamp(&self) -> String {
        format!("T+{:02}s", self.clock_ticks)
    }

    pub fn trim_command_timeline(&mut self) {
        while self.execution.command_timeline.len() > COMMAND_TIMELINE_LIMIT {
            self.execution.command_timeline.pop_back();
        }
    }

    pub fn is_snapshot_ready(&self) -> bool {
        matches!(self.snapshot_state, SnapshotBootstrapState::Ready)
    }

    pub fn snapshot_retry_backoff_ms(retry_count: u32) -> u64 {
        let shift = retry_count.saturating_sub(1).min(3);
        1_000u64.saturating_mul(2u64.saturating_pow(shift))
    }

    pub fn sync_runtime_snapshot(&mut self, snapshot: RuntimeSnapshot) {
        let ws_connected = self.connection.ws_connected;
        let reconnect_attempt = self.connection.reconnect_attempt;
        let reconnect_backoff_ms = self.connection.reconnect_backoff_ms;
        let ui = self.ui.clone();
        let system_events = self.system_events.clone();
        let alerts = self.risk.alerts.clone();
        let clock_ticks = self.clock_ticks;
        let next_command_seq = self.next_command_seq;

        *self = Self::from_snapshot(snapshot);
        self.connection.ws_connected = ws_connected;
        self.connection.reconnect_attempt = reconnect_attempt;
        self.connection.reconnect_backoff_ms = reconnect_backoff_ms;
        self.ui = ui;
        self.system_events = system_events;
        self.risk.alerts = alerts;
        self.clock_ticks = clock_ticks;
        self.next_command_seq = next_command_seq;
        self.dirty = DirtyFlags::all();
        self.immediate_render = true;
        self.trim_command_timeline();
    }
}

fn command_timeline_from_pending(command: &PendingCommand) -> CommandTimelineEntry {
    let stage = match command.status {
        CommandStatus::Pending => CommandTimelineStage::Pending,
        CommandStatus::Accepted => CommandTimelineStage::Accepted,
        CommandStatus::Completed => CommandTimelineStage::Ack,
        CommandStatus::Failed => CommandTimelineStage::Failed,
        CommandStatus::TimedOut => CommandTimelineStage::TimedOut,
    };
    CommandTimelineEntry {
        command_id: command.command_id.clone(),
        command: command.command,
        stage,
        summary: match stage {
            CommandTimelineStage::Pending => "Recovered pending command from snapshot.".into(),
            CommandTimelineStage::Accepted => {
                "Service accepted command before the client reconnected.".into()
            }
            CommandTimelineStage::Ack => {
                "Command already acknowledged before the client reconnected.".into()
            }
            CommandTimelineStage::Failed => {
                "Command already failed before the client reconnected.".into()
            }
            CommandTimelineStage::TimedOut => {
                "Command timed out before the client reconnected.".into()
            }
        },
        requested_at: command.requested_at.clone(),
        accepted_at: (stage == CommandTimelineStage::Accepted)
            .then(|| command.requested_at.clone()),
        finished_at: stage.is_terminal().then(|| command.requested_at.clone()),
        links: CommandLinks::default(),
        timeout_at_tick: None,
    }
}

fn command_timeline_from_record(command: &CommandRecord) -> CommandTimelineEntry {
    let stage = match command.status {
        CommandStatus::Pending => CommandTimelineStage::Pending,
        CommandStatus::Accepted => CommandTimelineStage::Accepted,
        CommandStatus::Completed => CommandTimelineStage::Ack,
        CommandStatus::Failed => CommandTimelineStage::Failed,
        CommandStatus::TimedOut => CommandTimelineStage::TimedOut,
    };
    CommandTimelineEntry {
        command_id: command.command_id.clone(),
        command: command.command,
        stage,
        summary: command.summary.clone(),
        requested_at: command.requested_at.clone(),
        accepted_at: command.accepted_at.clone(),
        finished_at: command.finished_at.clone(),
        links: command.links.clone(),
        timeout_at_tick: None,
    }
}

fn command_timeline_from_snapshot(snapshot: &RuntimeSnapshot) -> VecDeque<CommandTimelineEntry> {
    let mut timeline = snapshot
        .execution
        .recent_commands
        .iter()
        .map(command_timeline_from_record)
        .collect::<VecDeque<_>>();
    for pending in snapshot.execution.pending_commands.iter().rev() {
        timeline.push_front(command_timeline_from_pending(pending));
    }
    timeline
}
