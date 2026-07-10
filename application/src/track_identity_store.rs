use anyhow::Result;
use async_trait::async_trait;
use poise_core::track::{Instrument, TrackId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackIdentityGuardOutcome {
    Matched(TrackId),
    BoundNew(TrackId),
    AdoptedLegacyPnl(TrackId),
    AdoptedLegacyWithoutInstrumentEvidence(TrackId),
}

#[async_trait]
pub trait TrackIdentityStore: Send + Sync {
    /// Atomically binds or validates every configured track identity.
    ///
    /// Outcomes are returned in input order and are startup diagnostics only.
    async fn guard_track_identities(
        &self,
        identities: &[(TrackId, Instrument)],
    ) -> Result<Vec<TrackIdentityGuardOutcome>>;
}
