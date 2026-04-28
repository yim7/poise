use poise_core::events::DomainEvent;

use crate::execution_plan::TrackEffect;
use crate::mutation_frame::TrackMutationFrame;

#[derive(Debug, Clone)]
pub struct TrackTransition {
    pub frame: TrackMutationFrame,
    pub events: Vec<DomainEvent>,
    pub effects: Vec<TrackEffect>,
}
