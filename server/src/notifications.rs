use poise_engine::track::TrackId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackInternalNotification {
    TrackWriteCommitted {
        track_id: TrackId,
        recovery_anomaly_active: bool,
    },
    TrackEffectStateChanged {
        track_id: TrackId,
    },
}
