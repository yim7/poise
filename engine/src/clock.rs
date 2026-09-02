use chrono::{DateTime, Utc};

pub trait ClockPort: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
