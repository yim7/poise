use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use poise_engine::snapshot::TrackRuntimeSnapshot;
use poise_engine::track::TrackId;

use crate::{
    PreparedTrackRegistry, TrackListReadModel, TrackObservationService, TrackQueryStore,
    TrackReadModel, runtime_read_state_loader, track_read_source::TrackReadSource,
};

pub(crate) const DETAIL_EVENTS_LIMIT: usize = 20;
pub(crate) const DETAIL_EFFECTS_LIMIT: usize = 20;

#[derive(Clone)]
pub(crate) struct TrackReadSourceLoader {
    repository: Arc<dyn TrackQueryStore>,
    prepared_registry: Arc<PreparedTrackRegistry>,
    observation: Option<Arc<TrackObservationService>>,
}

impl TrackReadSourceLoader {
    pub(crate) fn new(
        repository: Arc<dyn TrackQueryStore>,
        prepared_registry: Arc<PreparedTrackRegistry>,
        observation: Option<Arc<TrackObservationService>>,
    ) -> Self {
        Self {
            repository,
            prepared_registry,
            observation,
        }
    }

    pub(crate) async fn list_track_read_models(&self) -> Result<Vec<TrackListReadModel>> {
        let mut snapshots = self.repository.list_track_snapshots().await?;
        snapshots.sort_by(|left, right| {
            left.snapshot
                .track_id
                .as_str()
                .cmp(right.snapshot.track_id.as_str())
        });

        let mut read_models = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let track_id = snapshot.snapshot.track_id.clone();
            let definition = self
                .prepared_registry
                .get(&track_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "missing prepared track definition for `{}`",
                        track_id.as_str()
                    )
                })?
                .read_definition();
            let runtime = runtime_read_state_loader::runtime_read_state_from_snapshot(
                &track_id,
                snapshot.snapshot,
                self.observation.clone(),
            )
            .await?;
            read_models.push(TrackListReadModel::from_parts(
                &definition,
                &runtime,
                snapshot.updated_at,
            ));
        }

        Ok(read_models)
    }

    pub(crate) async fn load_track_read_model(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackReadModel>> {
        Ok(self
            .load_track_read_source(track_id)
            .await?
            .map(TrackReadModel::from_source))
    }

    pub(crate) async fn load_track_read_source(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackReadSource>> {
        let Some(snapshot) = self.repository.load_track_snapshot(track_id).await? else {
            return Ok(None);
        };

        let recent_track_events = self
            .repository
            .list_recent_track_events(track_id, DETAIL_EVENTS_LIMIT)
            .await?;
        let recent_effects = self
            .repository
            .list_recent_track_effects(track_id, DETAIL_EFFECTS_LIMIT)
            .await?;

        Ok(Some(
            self.read_source_from_snapshot(
                track_id.clone(),
                snapshot.snapshot,
                snapshot.updated_at,
                recent_track_events,
                recent_effects,
            )
            .await?,
        ))
    }

    async fn read_source_from_snapshot(
        &self,
        track_id: TrackId,
        snapshot: TrackRuntimeSnapshot,
        updated_at: DateTime<Utc>,
        recent_track_events: Vec<crate::StoredTrackEvent>,
        recent_effects: Vec<crate::PersistedTrackEffect>,
    ) -> Result<TrackReadSource> {
        let definition = self
            .prepared_registry
            .get(&track_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "missing prepared track definition for `{}`",
                    track_id.as_str()
                )
            })?
            .read_definition();
        let runtime = runtime_read_state_loader::runtime_read_state_from_snapshot(
            &track_id,
            snapshot,
            self.observation.clone(),
        )
        .await?;

        Ok(TrackReadSource {
            definition,
            runtime,
            updated_at,
            recent_track_events,
            recent_effects,
        })
    }
}
