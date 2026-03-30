use poise_engine::track::TrackId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackInternalNotification {
    GridWriteCommitted {
        track_id: TrackId,
        recovery_anomaly_active: bool,
    },
    GridEffectStateChanged {
        track_id: TrackId,
    },
}
