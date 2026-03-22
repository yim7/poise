use crate::protocol::{
    CommandAccepted, CommandType, InstancesDirectory, RiskEvent, RuntimeSnapshot, ServerEvent,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolEvent {
    pub symbol: Option<String>,
    pub generation: Option<u64>,
    pub event: ServerEvent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    Protocol(ProtocolEvent),
    Input(InputEvent),
    System(SystemEvent),
    Command(CommandEvent),
    EffectResult(EffectResultEvent),
    LocalUi(LocalUiEvent),
}

impl From<ServerEvent> for ProtocolEvent {
    fn from(event: ServerEvent) -> Self {
        Self {
            symbol: None,
            generation: None,
            event,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key(KeyAction),
    Resize(u16, u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    ViewDashboard,
    ViewGrid,
    ViewMarket,
    ViewEvents,
    ToggleHelp,
    NextFocus,
    PrevFocus,
    NextInstance,
    PrevInstance,
    Pause,
    Resume,
    CancelAll,
    FlattenNow,
    ShutdownAfterFlatten,
    ToggleLocale,
    Confirm,
    Cancel,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SystemEvent {
    RenderTick,
    HealthTick,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandEvent {
    Request(CommandType),
}

#[derive(Debug, Clone, PartialEq)]
pub enum EffectResultEvent {
    InstancesLoaded(InstancesDirectory),
    InstancesFailed(String),
    SnapshotLoaded {
        symbol: String,
        generation: u64,
        snapshot: RuntimeSnapshot,
    },
    SnapshotFailed {
        symbol: String,
        generation: u64,
        error: String,
    },
    RiskEventsLoaded {
        symbol: String,
        generation: u64,
        alerts: Vec<RiskEvent>,
    },
    RiskEventsFailed {
        symbol: String,
        generation: u64,
        error: String,
    },
    WsConnected {
        symbol: String,
        generation: u64,
    },
    WsDisconnected {
        symbol: String,
        generation: u64,
        reason: String,
    },
    CommandAccepted {
        symbol: String,
        generation: u64,
        accepted: CommandAccepted,
    },
    CommandFailed {
        symbol: String,
        generation: u64,
        command_id: String,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum LocalUiEvent {
    OpenConfirm(CommandType),
    SelectInstance(String),
    ConfirmModal,
    CancelModal,
    ClearToast,
}
