use poise_core::events::DomainEvent;

use crate::execution_plan::TrackEffect;
use crate::runtime::TrackRuntime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitIntent {
    LiveOnly,
    Commit,
}

#[derive(Debug, Clone)]
pub struct TrackTransition {
    pub(crate) next_state: TrackRuntime,
    pub facts: Vec<DomainEvent>,
    pub effects: Vec<TrackEffect>,
    commit_intent: CommitIntent,
}

impl TrackTransition {
    pub fn commit(
        next_runtime: TrackRuntime,
        facts: Vec<DomainEvent>,
        effects: Vec<TrackEffect>,
    ) -> Self {
        Self {
            next_state: next_runtime,
            facts,
            effects,
            commit_intent: CommitIntent::Commit,
        }
    }

    pub fn live_only(next_runtime: TrackRuntime) -> Self {
        Self {
            next_state: next_runtime,
            facts: Vec::new(),
            effects: Vec::new(),
            commit_intent: CommitIntent::LiveOnly,
        }
    }

    pub fn next_state(&self) -> &TrackRuntime {
        &self.next_state
    }

    pub fn commit_intent(&self) -> CommitIntent {
        self.commit_intent
    }
}
