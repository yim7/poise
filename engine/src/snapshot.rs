use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use grid_core::events::ReplacementGateReason;
use grid_core::strategy::GridConfig;
use grid_core::types::Exposure;

use crate::grid::{GridId, Instrument};
use crate::runtime::{GridStatus, PendingOrder, RiskState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ObservedState {
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridRuntimeSnapshot {
    pub grid_id: GridId,
    pub instrument: Instrument,
    pub config: GridConfig,
    pub status: GridStatus,
    pub current_exposure: Exposure,
    pub target_exposure: Option<Exposure>,
    pub pending_order: Option<PendingOrder>,
    #[serde(default)]
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub risk: RiskState,
    pub observed: ObservedState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedGridState {
    pub snapshot: GridRuntimeSnapshot,
    pub events: Vec<grid_core::events::DomainEvent>,
}
