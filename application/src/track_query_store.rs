use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_core::track::TrackId;
use poise_engine::ledger::TrackLedgerState;

use crate::TrackControlState;
use crate::track_persistence::{PersistedTrackEffect, StoredTrackEvent};

#[async_trait]
pub trait TrackQueryStore: Send + Sync {
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
    async fn load_track_control_state(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackControlState>>;
    async fn load_track_ledger_state(&self, track_id: &TrackId)
    -> Result<Option<TrackLedgerState>>;
    /// Read-model freshness metadata only.
    ///
    /// This timestamp is backed by `persisted_track_presence`; it is not durable
    /// business truth and must not be used to decide startup correctness.
    async fn load_track_updated_at(&self, track_id: &TrackId) -> Result<Option<DateTime<Utc>>>;
}
