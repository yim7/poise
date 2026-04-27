use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use poise_engine::runtime::TrackRuntimeView;
use poise_engine::track::TrackId;

use crate::{
    PreparedTrackRegistry, TrackListReadModel, TrackObservationService, TrackQueryStore,
    TrackReadModel, track_read_source::TrackReadSource,
};

pub(crate) const DETAIL_EVENTS_LIMIT: usize = 20;
pub(crate) const DETAIL_EFFECTS_LIMIT: usize = 20;

#[derive(Clone)]
pub(crate) struct TrackReadSourceLoader {
    repository: Arc<dyn TrackQueryStore>,
    prepared_registry: Arc<PreparedTrackRegistry>,
    observation: Arc<TrackObservationService>,
}

impl TrackReadSourceLoader {
    pub(crate) fn new(
        repository: Arc<dyn TrackQueryStore>,
        prepared_registry: Arc<PreparedTrackRegistry>,
        observation: Arc<TrackObservationService>,
    ) -> Self {
        Self {
            repository,
            prepared_registry,
            observation,
        }
    }

    pub(crate) async fn list_track_read_models(&self) -> Result<Vec<TrackListReadModel>> {
        let mut read_models = Vec::new();
        for prepared in self.prepared_registry.iter() {
            let track_id = prepared.track_id().clone();
            let Some(runtime) = self
                .observation
                .track_runtime_view(track_id.as_str())
                .await?
            else {
                continue;
            };
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
            let updated_at = self
                .repository
                .load_track_updated_at(&track_id)
                .await?
                .unwrap_or_else(|| infer_runtime_updated_at(&runtime));
            read_models.push(TrackListReadModel::from_parts(
                &definition,
                &runtime,
                updated_at,
            ));
        }

        read_models.sort_by(|left, right| left.track_id.cmp(&right.track_id));
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
        let Some(runtime) = self
            .observation
            .track_runtime_view(track_id.as_str())
            .await?
        else {
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
        let updated_at = self
            .repository
            .load_track_updated_at(track_id)
            .await?
            .unwrap_or_else(|| infer_runtime_updated_at(&runtime));

        Ok(Some(
            self.read_source_from_runtime(
                track_id.clone(),
                runtime,
                updated_at,
                recent_track_events,
                recent_effects,
            )
            .await?,
        ))
    }

    async fn read_source_from_runtime(
        &self,
        track_id: TrackId,
        runtime: TrackRuntimeView,
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
        Ok(TrackReadSource {
            definition,
            runtime,
            updated_at,
            recent_track_events,
            recent_effects,
        })
    }
}

fn infer_runtime_updated_at(runtime: &TrackRuntimeView) -> DateTime<Utc> {
    runtime
        .last_tick_at
        .or(runtime.market_data_stale_since)
        .unwrap_or_else(Utc::now)
}
