use crate::protocol::CommandType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    FetchSnapshot,
    FetchSnapshotAfterDelay {
        retry_in_ms: u64,
    },
    FetchRiskEvents,
    ConnectWs,
    ReconnectWs {
        attempt: u32,
    },
    SendCommand {
        command: CommandType,
        command_id: String,
    },
    LogClientSideEvent(String),
}
