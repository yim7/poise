use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use poise_core::events::ReplacementGateReason;
use poise_core::strategy::TrackConfig;
use poise_core::types::Exposure;

use crate::runtime::{ExecutorState, RiskState, TrackStatus};
use crate::track::{Instrument, TrackId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ObservedState {
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_tick_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub market_data_stale_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRuntimeSnapshot {
    #[serde(alias = "grid_id")]
    pub track_id: TrackId,
    pub instrument: Instrument,
    pub config: TrackConfig,
    pub status: TrackStatus,
    pub current_exposure: Exposure,
    pub target_exposure: Option<Exposure>,
    #[serde(default)]
    pub manual_target_override: Option<Exposure>,
    pub executor_state: ExecutorState,
    #[serde(default)]
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub risk: RiskState,
    pub observed: ObservedState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedTrackState {
    pub snapshot: TrackRuntimeSnapshot,
    pub events: Vec<poise_core::events::DomainEvent>,
}
