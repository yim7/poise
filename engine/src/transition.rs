use poise_core::events::DomainEvent;

use crate::execution_plan::ExecutionAction;
use crate::snapshot::TrackRuntimeSnapshot;

pub type TrackEffect = ExecutionAction;

#[derive(Debug, Clone)]
pub struct TrackTransition {
    pub snapshot: TrackRuntimeSnapshot,
    pub events: Vec<DomainEvent>,
    pub effects: Vec<TrackEffect>,
}
