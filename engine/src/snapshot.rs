use chrono::{DateTime, Utc};
use poise_core::strategy::TrackConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use poise_core::types::Exposure;

use crate::execution_gate::ExecutionGateState;
use crate::executor::RecoveryAnomaly;
use crate::executor::binding::LiveOrderBinding;
use crate::executor::ledger::BoundaryLedgerAnchorSnapshot;
use crate::ledger::TrackLedgerState;
use crate::runtime::{
    ExecutorState, RecentTerminalOrder, RiskState, StrategyPriceStatus, TrackState,
};
use crate::track::{Instrument, TrackId};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRestoreRevision(String);

impl TrackRestoreRevision {
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
pub struct TrackRuntimeSnapshot {
    pub track_id: TrackId,
    pub restore_revision: TrackRestoreRevision,
    pub runtime_state: TrackState,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub executor_state: ExecutorState,
    pub execution_gate_state: ExecutionGateState,
    pub ledger_state: TrackLedgerState,
    pub risk: RiskState,
    pub observed: ObservedState,
}

impl TrackRuntimeSnapshot {
    pub fn to_document(&self) -> TrackRuntimeSnapshotDocument {
        TrackRuntimeSnapshotDocument {
            track_id: self.track_id.clone(),
            restore_revision: self.restore_revision.clone(),
            runtime_state: self.runtime_state.clone(),
            current_exposure: self.current_exposure.clone(),
            desired_exposure: self.desired_exposure.clone(),
            executor_state: self.executor_state.to_document(),
            execution_gate_state: self.execution_gate_state.clone(),
            ledger_state: self.ledger_state.clone(),
            risk: self.risk.clone(),
            observed: self.observed.clone(),
        }
    }

    pub fn from_document(document: TrackRuntimeSnapshotDocument) -> Self {
        document.into_runtime_snapshot()
    }

    pub fn status(&self) -> crate::runtime::TrackStatus {
        self.runtime_state.status()
    }

    pub fn manual_target_override(&self) -> Option<Exposure> {
        self.runtime_state.manual_target_override()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutorStateDocument {
    pub ledger_state: BoundaryLedgerAnchorSnapshot,
    pub bindings: Vec<LiveOrderBinding>,
    pub recent_terminal_orders: Vec<RecentTerminalOrder>,
    #[serde(default)]
    pub recovery_anomaly: Option<RecoveryAnomaly>,
}

impl ExecutorState {
    pub fn to_document(&self) -> ExecutorStateDocument {
        ExecutorStateDocument {
            ledger_state: self.ledger_state.to_anchor_snapshot(),
            bindings: self.bindings.clone(),
            recent_terminal_orders: self.recent_terminal_orders.clone(),
            recovery_anomaly: self.recovery_anomaly.clone(),
        }
    }
}

impl ExecutorStateDocument {
    pub fn into_runtime_state(self) -> ExecutorState {
        ExecutorState {
            ledger_state: self.ledger_state.into_runtime_state(),
            bindings: self.bindings,
            recent_terminal_orders: self.recent_terminal_orders,
            recovery_anomaly: self.recovery_anomaly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRuntimeSnapshotDocument {
    pub track_id: TrackId,
    pub restore_revision: TrackRestoreRevision,
    pub runtime_state: TrackState,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub executor_state: ExecutorStateDocument,
    #[serde(default)]
    pub execution_gate_state: ExecutionGateState,
    pub ledger_state: TrackLedgerState,
    pub risk: RiskState,
    pub observed: ObservedState,
}

impl TrackRuntimeSnapshotDocument {
    pub fn into_runtime_snapshot(self) -> TrackRuntimeSnapshot {
        TrackRuntimeSnapshot {
            track_id: self.track_id,
            restore_revision: self.restore_revision,
            runtime_state: self.runtime_state,
            current_exposure: self.current_exposure,
            desired_exposure: self.desired_exposure,
            executor_state: self.executor_state.into_runtime_state(),
            execution_gate_state: self.execution_gate_state,
            ledger_state: self.ledger_state,
            risk: self.risk,
            observed: self.observed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedTrackState {
    pub snapshot: TrackRuntimeSnapshotDocument,
    pub events: Vec<poise_core::events::DomainEvent>,
}
