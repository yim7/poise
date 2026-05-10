use poise_core::types::Exposure;
use poise_engine::runtime::{AutoState, ControlState, ManualState, TerminationCause, TrackState};
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

    pub fn from_runtime_state_for_write(track_state: &TrackState) -> Self {
        match track_state {
            TrackState::WaitingMarketData => Self::Enabled {
                mode: PersistedControlMode::Automatic,
            },
            TrackState::Running(control) => Self::Enabled {
                mode: PersistedControlMode::from_runtime_control_state(control),
            },
            TrackState::Paused { suspended } => Self::Paused {
                resume_mode: PersistedControlMode::from_runtime_control_state(suspended),
            },
            TrackState::Terminated { cause } => Self::Terminated {
                cause: cause.clone(),
            },
        }
    }

    pub fn to_startup_runtime_state(&self) -> TrackState {
        match self {
            Self::Enabled {
                mode: PersistedControlMode::Automatic,
            } => TrackState::WaitingMarketData,
            Self::Enabled {
                mode: PersistedControlMode::ManualFlatten,
            } => TrackState::Running(ControlState::Manual(ManualState::Flattened)),
            Self::Enabled {
                mode: PersistedControlMode::ManualTargetOverride { target },
            } => TrackState::Running(ControlState::Manual(ManualState::TargetOverride {
                target: target.clone(),
            })),
            Self::Paused { resume_mode } => TrackState::Paused {
                suspended: resume_mode.to_runtime_control_state(),
            },
            Self::Terminated { cause } => TrackState::Terminated {
                cause: cause.clone(),
            },
        }
    }

    fn resume_mode(&self) -> PersistedControlMode {
        match self {
            Self::Enabled { mode } | Self::Paused { resume_mode: mode } => mode.clone(),
            Self::Terminated { .. } => PersistedControlMode::Automatic,
        }
    }
}

impl PersistedControlMode {
    fn from_runtime_control_state(control_state: &ControlState) -> Self {
        match control_state {
            ControlState::Automatic(
                AutoState::FollowingBand
                | AutoState::AcquiringRiskExposure { .. }
                | AutoState::Frozen { .. }
                | AutoState::FlattenPending { .. }
                | AutoState::Flattening { .. },
            ) => Self::Automatic,
            ControlState::Manual(ManualState::Flattened) => Self::ManualFlatten,
            ControlState::Manual(ManualState::TargetOverride { target }) => {
                Self::ManualTargetOverride {
                    target: target.clone(),
                }
            }
        }
    }

    fn to_runtime_control_state(&self) -> ControlState {
        match self {
            Self::Automatic => ControlState::Automatic(AutoState::FollowingBand),
            Self::ManualFlatten => ControlState::Manual(ManualState::Flattened),
            Self::ManualTargetOverride { target } => {
                ControlState::Manual(ManualState::TargetOverride {
                    target: target.clone(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use poise_core::types::Exposure;
    use poise_engine::runtime::{
        AutoState, BandTerminationCause, ControlState, TerminationCause, TrackState,
    };

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
    fn session_transient_states_fold_into_closed_control_state_on_write() {
        assert_eq!(
            TrackControlState::from_runtime_state_for_write(&TrackState::WaitingMarketData),
            TrackControlState::Enabled {
                mode: PersistedControlMode::Automatic,
            }
        );
        assert_eq!(
            TrackControlState::from_runtime_state_for_write(&TrackState::Running(
                ControlState::Automatic(AutoState::Frozen {
                    target_anchor: Exposure(1.0),
                }),
            )),
            TrackControlState::Enabled {
                mode: PersistedControlMode::Automatic,
            }
        );
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
