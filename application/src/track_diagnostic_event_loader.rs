use std::sync::Arc;

use anyhow::Result;
use poise_engine::track::TrackId;

use crate::{StoredTrackEvent, TrackQueryStore};

pub(crate) const DIAGNOSTIC_EVENTS_LIMIT: usize = 20;

#[derive(Clone)]
pub(crate) struct TrackDiagnosticEventLoader {
    repository: Arc<dyn TrackQueryStore>,
}

impl TrackDiagnosticEventLoader {
    pub(crate) fn new(repository: Arc<dyn TrackQueryStore>) -> Self {
        Self { repository }
    }

    pub(crate) async fn load_recent_track_events(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<Vec<StoredTrackEvent>>> {
        let Some(_) = self.repository.load_track_snapshot(track_id).await? else {
            return Ok(None);
        };

        Ok(Some(
            self.repository
                .list_recent_track_events(track_id, DIAGNOSTIC_EVENTS_LIMIT)
                .await?,
        ))
    }
}
