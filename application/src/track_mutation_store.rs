use anyhow::Result;
use async_trait::async_trait;
use poise_core::events::DomainEvent;
use poise_engine::snapshot::TrackRuntimeSnapshot;
use poise_engine::transition::TrackEffect;

use crate::track_persistence::{CommittedTrackWrite, EffectStatusUpdate, TrackPersistentState};

#[async_trait]
pub trait TrackMutationStore: Send + Sync {
    async fn save_transition_with_effect_status(
        &self,
        id: &str,
        state: &TrackRuntimeSnapshot,
        events: &[DomainEvent],
        effects: &[TrackEffect],
        effect_status_update: Option<&EffectStatusUpdate>,
    ) -> Result<CommittedTrackWrite>;

    async fn save_transition(
        &self,
        id: &str,
        state: &TrackRuntimeSnapshot,
        events: &[DomainEvent],
        effects: &[TrackEffect],
    ) -> Result<CommittedTrackWrite> {
        self.save_transition_with_effect_status(id, state, events, effects, None)
            .await
    }

    async fn load_track_state(&self, id: &str) -> Result<Option<TrackRuntimeSnapshot>>;
    async fn list_track_events(&self, id: &str) -> Result<Vec<DomainEvent>>;

    async fn save_track_persistent_state(&self, _state: &TrackPersistentState) -> Result<()> {
        Ok(())
    }
}
