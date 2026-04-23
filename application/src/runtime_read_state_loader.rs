use anyhow::Result;
use poise_engine::track::TrackId;

use crate::TrackObservationService;
use crate::track_read_source::TrackRuntimeReadState;

pub(crate) async fn load_runtime_read_state(
    observation: &TrackObservationService,
    track_id: &TrackId,
) -> Result<Option<TrackRuntimeReadState>> {
    let Some(snapshot) = observation.track_snapshot(track_id.as_str()).await? else {
        return Ok(None);
    };

    Ok(Some(
        runtime_read_state_from_snapshot(track_id, snapshot, observation).await?,
    ))
}

pub(crate) async fn runtime_read_state_from_snapshot(
    track_id: &TrackId,
    snapshot: poise_engine::snapshot::TrackRuntimeSnapshot,
    observation: &TrackObservationService,
) -> Result<TrackRuntimeReadState> {
    let live_view = observation.track_live_view(track_id.as_str()).await?;
    Ok(TrackRuntimeReadState::from_parts(snapshot, live_view))
}
