use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::persisted_runtime::TrackRestoreRevision;
use poise_core::types::Exposure;

use crate::execution_gate::ExecutionGateState;
use crate::ledger::TrackLedgerState;
use crate::runtime::{ExecutorState, RiskState, StrategyPriceStatus, TrackState};
use crate::track::TrackId;

fn strategy_price_status_is_stale(status: &StrategyPriceStatus) -> bool {
    matches!(status, StrategyPriceStatus::Stale)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ObservedState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_price: Option<f64>,
    #[serde(default, skip_serializing_if = "strategy_price_status_is_stale")]
    pub strategy_price_status: StrategyPriceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark_price: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_bid: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_ask: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tick_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub market_data_stale_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRuntimeSnapshot {
    pub track_id: TrackId,
    pub restore_revision: TrackRestoreRevision,
    pub runtime_state: TrackState,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub executor_state: ExecutorState,
    #[serde(default)]
    pub execution_gate_state: ExecutionGateState,
    pub ledger_state: TrackLedgerState,
    pub risk: RiskState,
    pub observed: ObservedState,
}

impl TrackRuntimeSnapshot {
    pub fn status(&self) -> crate::runtime::TrackStatus {
        self.runtime_state.status()
    }

    pub fn manual_target_override(&self) -> Option<Exposure> {
        self.runtime_state.manual_target_override()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedTrackState {
    pub snapshot: TrackRuntimeSnapshot,
    pub events: Vec<poise_core::events::DomainEvent>,
}
