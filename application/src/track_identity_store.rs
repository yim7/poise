use anyhow::Result;
use async_trait::async_trait;
use poise_core::track::{Instrument, TrackId};

#[async_trait]
pub trait TrackIdentityStore: Send + Sync {
    /// Atomically binds or validates every configured track identity.
    ///
    async fn guard_track_identities(&self, identities: &[(TrackId, Instrument)]) -> Result<()>;
}
