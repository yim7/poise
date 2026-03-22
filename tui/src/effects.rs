use crate::protocol::CommandType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    FetchInstances,
    FetchInstancesAfterDelay {
        retry_in_ms: u64,
    },
    UseInstance {
        symbol: String,
        generation: u64,
    },
    FetchSnapshot {
        symbol: String,
        generation: u64,
    },
    FetchSnapshotAfterDelay {
        symbol: String,
        generation: u64,
        retry_in_ms: u64,
    },
    FetchRiskEvents {
        symbol: String,
        generation: u64,
    },
    ConnectWs {
        symbol: String,
        generation: u64,
    },
    ReconnectWs {
        symbol: String,
        generation: u64,
        attempt: u32,
    },
    SendCommand {
        symbol: String,
        generation: u64,
        command: CommandType,
        command_id: String,
    },
    LogClientSideEvent(String),
}
