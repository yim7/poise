use anyhow::Result;
use async_trait::async_trait;
use poise_engine::track::TrackId;

use crate::track_persistence::{
    PersistedTrackEffect, StoredTrackEvent, StoredTrackSnapshot, TrackPersistentState,
};

#[async_trait]
pub trait TrackQueryStore: Send + Sync {
    async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>>;
    async fn load_track_snapshot(&self, track_id: &TrackId) -> Result<Option<StoredTrackSnapshot>>;
    async fn list_recent_track_events(
        &self,
        track_id: &TrackId,
        limit: usize,
    ) -> Result<Vec<StoredTrackEvent>>;
    async fn list_recent_track_effects(
        &self,
        track_id: &TrackId,
        limit: usize,
    ) -> Result<Vec<PersistedTrackEffect>>;

    async fn load_track_persistent_state(
        &self,
        _track_id: &TrackId,
    ) -> Result<Option<TrackPersistentState>> {
        Ok(None)
    }
}
