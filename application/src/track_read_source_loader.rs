use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use poise_core::track::TrackId;
use poise_engine::runtime::TrackRuntimeView;

use crate::{
    TrackListReadModel, TrackQueryStore, TrackReadModel, TradingRuntime,
    track_read_source::TrackReadSource, trading_runtime::CommittedTrackSnapshot,
};

pub(crate) const DETAIL_EVENTS_LIMIT: usize = 20;
pub(crate) const DETAIL_EFFECTS_LIMIT: usize = 20;

#[derive(Clone)]
pub(crate) struct TrackReadSourceLoader {
    repository: Arc<dyn TrackQueryStore>,
    runtime: Arc<TradingRuntime>,
}

impl TrackReadSourceLoader {
    pub(crate) fn new(repository: Arc<dyn TrackQueryStore>, runtime: Arc<TradingRuntime>) -> Self {
        Self {
            repository,
            runtime,
        }
    }

    pub(crate) async fn list_track_read_models(&self) -> Result<Vec<TrackListReadModel>> {
        let mut read_models = Vec::new();
        for snapshot in self.runtime.committed_track_snapshots().await? {
            let track_id = snapshot.definition.track_id().clone();
            let updated_at = self
                .repository
                .load_track_updated_at(&track_id)
                .await?
                .unwrap_or_else(|| infer_runtime_updated_at(&snapshot.runtime));
            read_models.push(TrackListReadModel::from_parts(
                &snapshot.definition,
                &snapshot.runtime,
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
        let Some(snapshot) = self.runtime.committed_track_snapshot(track_id).await? else {
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
            .unwrap_or_else(|| infer_runtime_updated_at(&snapshot.runtime));

        Ok(Some(self.read_source_from_runtime(
            snapshot,
            updated_at,
            recent_track_events,
            recent_effects,
        )))
    }

    fn read_source_from_runtime(
        &self,
        snapshot: CommittedTrackSnapshot,
        updated_at: DateTime<Utc>,
        recent_track_events: Vec<crate::StoredTrackEvent>,
        recent_effects: Vec<crate::PersistedTrackEffect>,
    ) -> TrackReadSource {
        TrackReadSource {
            definition: snapshot.definition,
            runtime: snapshot.runtime,
            updated_at,
            recent_track_events,
            recent_effects,
        }
    }
}

fn infer_runtime_updated_at(runtime: &TrackRuntimeView) -> DateTime<Utc> {
    runtime
        .last_tick_at
        .or(runtime.market_data_stale_since)
        .unwrap_or_else(Utc::now)
}
