use poise_engine::track::TrackId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationNotification {
    TrackChanged { track_id: TrackId },
    AccountChanged,
}
