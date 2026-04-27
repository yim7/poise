use std::sync::Arc;

use anyhow::Result;
use poise_engine::track::TrackId;

use crate::{StoredTrackEvent, TrackObservationService, TrackQueryStore};

pub(crate) const DIAGNOSTIC_EVENTS_LIMIT: usize = 20;

#[derive(Clone)]
pub(crate) struct TrackDiagnosticEventLoader {
    repository: Arc<dyn TrackQueryStore>,
    observation: Arc<TrackObservationService>,
}

impl TrackDiagnosticEventLoader {
    pub(crate) fn new(
        repository: Arc<dyn TrackQueryStore>,
        observation: Arc<TrackObservationService>,
    ) -> Self {
        Self {
            repository,
            observation,
        }
    }

    pub(crate) async fn load_recent_track_events(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<Vec<StoredTrackEvent>>> {
        let Some(_) = self
            .observation
            .track_runtime_view(track_id.as_str())
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(
            self.repository
                .list_recent_track_events(track_id, DIAGNOSTIC_EVENTS_LIMIT)
                .await?,
        ))
    }
}
