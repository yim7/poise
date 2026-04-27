use chrono::{DateTime, Utc};
use poise_core::strategy::TrackConfig;
use sha2::{Digest, Sha256};

use poise_core::types::Exposure;

use crate::execution_gate::ExecutionGateState;
use crate::ledger::TrackLedgerState;
use crate::runtime::{ExecutorState, RiskState, StrategyPriceStatus, TrackState};
use crate::track::{Instrument, TrackId};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameObservedState {
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
    pub last_tick_at: Option<DateTime<Utc>>,
    pub market_data_stale_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMutationFrameRevision(String);

impl TrackMutationFrameRevision {
    pub fn for_track(instrument: &Instrument, track_config: &TrackConfig) -> Self {
        let payload = serde_json::json!({
            "instrument": instrument,
            "track_config": track_config,
        });
        let mut hasher = Sha256::new();
        hasher.update(payload.to_string().as_bytes());
        Self(format!("{:x}", hasher.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackMutationFrame {
    pub track_id: TrackId,
    pub frame_revision: TrackMutationFrameRevision,
    pub runtime_state: TrackState,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub executor_state: ExecutorState,
    pub execution_gate_state: ExecutionGateState,
    pub ledger_state: TrackLedgerState,
    pub risk: RiskState,
    pub observed: FrameObservedState,
}

impl TrackMutationFrame {
    pub fn status(&self) -> crate::runtime::TrackStatus {
        self.runtime_state.status()
    }

    pub fn manual_target_override(&self) -> Option<Exposure> {
        self.runtime_state.manual_target_override()
    }
}
