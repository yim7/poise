use chrono::{DateTime, Utc};
use poise_core::events::DomainEvent;
use poise_core::track::TrackId;
use poise_engine::execution_plan::TrackEffect;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackActivityKind {
    ConfigurationPreparing,
    ConfigurationWaitingForOrders,
    ConfigurationActivating,
    ConfigurationActive,
    ConfigurationFailed,
    ConfigurationInterrupted,
    CommandPause,
    CommandResume,
    CommandTerminate,
    CommandFlatten,
}

impl TrackActivityKind {
    pub fn level(self) -> crate::TrackActivityLevel {
        match self {
            Self::ConfigurationFailed | Self::ConfigurationInterrupted => {
                crate::TrackActivityLevel::Error
            }
            _ => crate::TrackActivityLevel::Info,
        }
    }

    pub fn message(self, reference_id: Option<&str>, change_fields: &[String]) -> String {
        let base = match self {
            Self::ConfigurationPreparing => "configuration application accepted",
            Self::ConfigurationWaitingForOrders => "configuration application waiting for orders",
            Self::ConfigurationActivating => "configuration application activating",
            Self::ConfigurationActive => "configuration application active",
            Self::ConfigurationFailed => "configuration application failed",
            Self::ConfigurationInterrupted => "configuration application interrupted",
            Self::CommandPause => "manual pause submitted",
            Self::CommandResume => "manual resume submitted",
            Self::CommandTerminate => "manual terminate submitted",
            Self::CommandFlatten => "manual flatten submitted",
        };
        let base = reference_id
            .map(|reference_id| format!("{base} (application {reference_id})"))
            .unwrap_or_else(|| base.to_string());
        if change_fields.is_empty() {
            base
        } else {
            format!("{base}: {}", change_fields.join(", "))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredTrackActivity {
    pub id: i64,
    pub track_id: TrackId,
    pub kind: TrackActivityKind,
    pub reference_id: Option<String>,
    pub change_fields: Vec<String>,
    pub created_at: DateTime<Utc>,
}

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
