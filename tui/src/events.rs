use crate::protocol::{CommandAccepted, CommandType, RiskEvent, RuntimeSnapshot, ServerEvent};

#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    Protocol(ServerEvent),
    Input(InputEvent),
    System(SystemEvent),
    Command(CommandEvent),
    EffectResult(EffectResultEvent),
    LocalUi(LocalUiEvent),
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
    Pause,
    Resume,
    CancelAll,
    FlattenNow,
    ShutdownAfterFlatten,
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
    SnapshotLoaded(RuntimeSnapshot),
    SnapshotFailed(String),
    RiskEventsLoaded(Vec<RiskEvent>),
    RiskEventsFailed(String),
    WsConnected,
    WsDisconnected(String),
    CommandAccepted(CommandAccepted),
    CommandFailed { command_id: String, error: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum LocalUiEvent {
    OpenConfirm(CommandType),
    ConfirmModal,
    CancelModal,
    ClearToast,
}
