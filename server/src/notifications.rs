use poise_engine::track::TrackId;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerNotification {
    TrackChanged { track_id: TrackId },
    AccountChanged,
}
