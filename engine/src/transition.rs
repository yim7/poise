use poise_core::events::DomainEvent;

use crate::execution_plan::ExecutionAction;
use crate::mutation_frame::TrackMutationFrame;

pub type TrackEffect = ExecutionAction;

#[derive(Debug, Clone)]
pub struct TrackTransition {
    pub frame: TrackMutationFrame,
    pub events: Vec<DomainEvent>,
    pub effects: Vec<TrackEffect>,
}
