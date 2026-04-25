use anyhow::Result;
use async_trait::async_trait;
use poise_engine::track::TrackId;

use crate::track_persistence::PersistedTrackEffect;

#[async_trait]
pub trait TrackEffectStore: Send + Sync {
    async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>>;
    async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>>;
    async fn list_all_pending_effects_for_track(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<PersistedTrackEffect>>;
    async fn list_session_reset_effects_for_track(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<PersistedTrackEffect>>;
    async fn list_pending_submit_effects_for_track(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<PersistedTrackEffect>>;
    async fn list_pending_submit_effects_for_track_batch(
        &self,
        track_id: &TrackId,
        batch_id: &str,
    ) -> Result<Vec<PersistedTrackEffect>>;
}
