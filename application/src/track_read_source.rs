use chrono::{DateTime, Utc};
use poise_engine::runtime::TrackRuntimeView;

use crate::track_definition::TrackReadDefinition;
use crate::track_persistence::{PersistedTrackEffect, StoredTrackEvent};

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadSource {
    pub definition: TrackReadDefinition,
    pub runtime: TrackRuntimeView,
    pub updated_at: DateTime<Utc>,
    pub recent_track_events: Vec<StoredTrackEvent>,
    pub recent_effects: Vec<PersistedTrackEffect>,
}
