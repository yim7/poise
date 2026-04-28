use chrono::{DateTime, Utc};
use poise_core::track::TrackDefinition;
use poise_engine::runtime::TrackRuntimeView;

use crate::track_persistence::{PersistedTrackEffect, StoredTrackEvent};

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadSource {
    pub definition: TrackDefinition,
    pub runtime: TrackRuntimeView,
    pub updated_at: DateTime<Utc>,
    pub recent_track_events: Vec<StoredTrackEvent>,
    pub recent_effects: Vec<PersistedTrackEffect>,
}
