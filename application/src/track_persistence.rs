use chrono::{DateTime, Utc};
use poise_core::events::DomainEvent;
use poise_engine::track::TrackId;
use poise_engine::transition::TrackEffect;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredTrackEvent {
    pub id: i64,
    pub track_id: TrackId,
    pub event: DomainEvent,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommittedTrackWrite {
    pub track_id: TrackId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectStatusUpdate {
    pub effect_id: String,
    pub status: EffectStatus,
    pub attempt_delta: u32,
    pub last_error: Option<String>,
}

impl EffectStatusUpdate {
    pub fn succeeded(effect_id: impl Into<String>) -> Self {
        Self {
            effect_id: effect_id.into(),
            status: EffectStatus::Succeeded,
            attempt_delta: 0,
            last_error: None,
        }
    }

    pub fn superseded(effect_id: impl Into<String>) -> Self {
        Self {
            effect_id: effect_id.into(),
            status: EffectStatus::Superseded,
            attempt_delta: 0,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectJournalEntry {
    pub effect_id: String,
    pub track_id: TrackId,
    pub batch_id: String,
    pub sequence: u32,
    pub effect: TrackEffect,
    pub created_at: DateTime<Utc>,
}

impl EffectJournalEntry {
    pub fn from_session_effect(effect: &crate::SessionTrackEffect) -> Self {
        Self {
            effect_id: effect.effect_id.clone(),
            track_id: effect.track_id.clone(),
            batch_id: effect.batch_id.clone(),
            sequence: effect.sequence,
            effect: effect.effect.clone(),
            created_at: effect.created_at,
        }
    }

    pub fn from_session_effects(effects: &[crate::SessionTrackEffect]) -> Vec<Self> {
        effects.iter().map(Self::from_session_effect).collect()
    }
}

impl From<EffectJournalEntry> for PersistedTrackEffect {
    fn from(entry: EffectJournalEntry) -> Self {
        Self {
            effect_id: entry.effect_id,
            track_id: entry.track_id,
            batch_id: entry.batch_id,
            sequence: entry.sequence,
            effect: entry.effect,
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: entry.created_at,
            updated_at: entry.created_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedTrackEffect {
    pub effect_id: String,
    pub track_id: TrackId,
    pub batch_id: String,
    pub sequence: u32,
    pub effect: TrackEffect,
    pub status: EffectStatus,
    pub attempt_count: u32,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    Pending,
    Executing,
    Succeeded,
    Superseded,
    Failed,
}

impl EffectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Executing => "executing",
            Self::Succeeded => "succeeded",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
        }
    }

    pub fn unblocks_follow_up(self) -> bool {
        matches!(self, Self::Succeeded | Self::Superseded)
    }
}
