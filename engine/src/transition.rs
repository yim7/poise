use poise_core::events::DomainEvent;

use crate::execution_plan::TrackEffect;
use crate::runtime::TrackRuntime;

#[derive(Debug, Clone)]
pub struct TrackTransition {
    pub(crate) next_state: TrackRuntime,
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
            next_state: next_runtime,
            events,
            effects,
        }
    }

    pub fn next_state(&self) -> &TrackRuntime {
        &self.next_state
    }
}
