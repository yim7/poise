use std::sync::Arc;

use anyhow::Result;
use poise_engine::snapshot::TrackRuntimeSnapshot;
use poise_engine::track::TrackId;

use crate::track_read_source::TrackRuntimeReadState;
use crate::{TrackObservationService, TrackQueryStore};

pub(crate) async fn load_runtime_read_state(
    query_store: Arc<dyn TrackQueryStore>,
    observation: Option<Arc<TrackObservationService>>,
    track_id: &TrackId,
) -> Result<Option<TrackRuntimeReadState>> {
    let Some(snapshot) = query_store.load_track_snapshot(track_id).await? else {
        return Ok(None);
    };

    Ok(Some(
        runtime_read_state_from_snapshot(track_id, snapshot.snapshot, observation).await?,
    ))
}

pub(crate) async fn runtime_read_state_from_snapshot(
    track_id: &TrackId,
    snapshot: TrackRuntimeSnapshot,
    observation: Option<Arc<TrackObservationService>>,
) -> Result<TrackRuntimeReadState> {
    Ok(match observation {
        Some(observation) => {
            let live_view = observation.track_live_view(track_id.as_str()).await?;
            TrackRuntimeReadState::from_parts(snapshot, live_view)
        }
        None => TrackRuntimeReadState::from_snapshot(snapshot),
    })
}
