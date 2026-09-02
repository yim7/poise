use poise_core::events::DomainEvent;

use crate::execution_plan::TrackEffect;
use crate::mutation_frame::TrackMutationFrame;
use crate::runtime::TrackRuntime;

#[derive(Debug, Clone)]
pub struct TrackTransition {
    pub frame: TrackMutationFrame,
    pub(crate) next_runtime: TrackRuntime,
    pub events: Vec<DomainEvent>,
    pub effects: Vec<TrackEffect>,
}

impl TrackTransition {
    pub fn new(
        next_runtime: TrackRuntime,
        events: Vec<DomainEvent>,
        effects: Vec<TrackEffect>,
    ) -> Self {
        Self {
            frame: next_runtime.mutation_frame(),
            next_runtime,
            events,
            effects,
        }
    }

    pub fn next_runtime(&self) -> &TrackRuntime {
        &self.next_runtime
    }
}
