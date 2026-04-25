use anyhow::Result;
use async_trait::async_trait;

use crate::track_persistence::{EffectJournalEntry, EffectStatusUpdate};

#[async_trait]
pub trait TrackEffectJournal: Send + Sync {
    async fn append_entries(&self, entries: &[EffectJournalEntry]) -> Result<()>;
    async fn record_effect_outcomes(&self, outcomes: &[EffectStatusUpdate]) -> Result<()>;
}
