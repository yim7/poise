use poise_core::types::Exposure;
use poise_engine::runtime::{AutoState, TerminationCause};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrackControlState {
    Enabled { mode: PersistedControlMode },
    Paused { resume_mode: PersistedControlMode },
    Terminated { cause: TerminationCause },
}

impl Default for TrackControlState {
    fn default() -> Self {
        Self::Enabled {
            mode: PersistedControlMode::Automatic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PersistedControlMode {
    Automatic,
    ManualFlatten,
    ManualTargetOverride { target: Exposure },
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrackControlCommand {
    Enable,
    Pause,
    Resume,
    Terminate { cause: TerminationCause },
    ManualFlatten,
    ManualTargetOverride { target: Exposure },
    Automatic,
}

impl TrackControlState {
    pub fn from_command(current: Self, command: TrackControlCommand) -> Self {
        match command {
            TrackControlCommand::Enable | TrackControlCommand::Automatic => Self::Enabled {
                mode: PersistedControlMode::Automatic,
            },
            TrackControlCommand::Pause => Self::Paused {
                resume_mode: current.resume_mode(),
            },
            TrackControlCommand::Resume => Self::Enabled {
                mode: current.resume_mode(),
            },
            TrackControlCommand::Terminate { cause } => Self::Terminated { cause },
            TrackControlCommand::ManualFlatten => Self::Enabled {
                mode: PersistedControlMode::ManualFlatten,
            },
            TrackControlCommand::ManualTargetOverride { target } => Self::Enabled {
                mode: PersistedControlMode::ManualTargetOverride { target },
            },
        }
    }

    pub fn from_auto_state(auto_state: AutoState) -> Option<Self> {
        match auto_state {
            AutoState::FollowingBand => Some(Self::Enabled {
                mode: PersistedControlMode::Automatic,
            }),
            AutoState::Frozen { .. }
            | AutoState::FlattenPending { .. }
            | AutoState::Flattening { .. } => None,
        }
    }

    fn resume_mode(&self) -> PersistedControlMode {
        match self {
            Self::Enabled { mode } | Self::Paused { resume_mode: mode } => mode.clone(),
            Self::Terminated { .. } => PersistedControlMode::Automatic,
        }
    }
}

#[cfg(test)]
mod tests {
    use poise_core::types::Exposure;
    use poise_engine::runtime::{AutoState, BandTerminationCause, TerminationCause};

    use super::{PersistedControlMode, TrackControlCommand, TrackControlState};

    #[test]
    fn control_state_is_a_closed_persistent_set() {
        assert_eq!(
            TrackControlState::default(),
            TrackControlState::Enabled {
                mode: PersistedControlMode::Automatic,
            }
        );

        assert_eq!(
            TrackControlState::from_command(
                TrackControlState::default(),
                TrackControlCommand::ManualTargetOverride {
                    target: Exposure(2.0),
                },
            ),
            TrackControlState::Enabled {
                mode: PersistedControlMode::ManualTargetOverride {
                    target: Exposure(2.0),
                },
            }
        );

        assert_eq!(
            TrackControlState::from_command(
                TrackControlState::Enabled {
                    mode: PersistedControlMode::ManualFlatten,
                },
                TrackControlCommand::Pause,
            ),
            TrackControlState::Paused {
                resume_mode: PersistedControlMode::ManualFlatten,
            }
        );
    }

    #[test]
    fn session_transient_states_are_not_persistable_control_state() {
        assert!(TrackControlState::from_auto_state(AutoState::FollowingBand).is_some());
        assert!(TrackControlState::from_auto_state(AutoState::Frozen {
            target_anchor: Exposure(1.0),
        })
        .is_none());
        assert!(TrackControlState::from_auto_state(AutoState::FlattenPending {
            target_anchor: Exposure(1.0),
            boundary: poise_core::strategy::BandBoundary::Below,
        })
        .is_none());
        assert!(TrackControlState::from_auto_state(AutoState::Flattening {
            boundary: poise_core::strategy::BandBoundary::Above,
        })
        .is_none());
    }

    #[test]
    fn terminated_control_state_keeps_business_cause() {
        let cause = TerminationCause::Band(BandTerminationCause::OutOfRange);

        assert_eq!(
            TrackControlState::from_command(
                TrackControlState::default(),
                TrackControlCommand::Terminate {
                    cause: cause.clone(),
                },
            ),
            TrackControlState::Terminated { cause }
        );
    }
}
