use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use poise_core::events::ReplacementGateReason;
use poise_core::strategy::TrackConfig;
use poise_core::types::Exposure;

use crate::ledger::TrackLedgerState;
use crate::runtime::{AccountCapacityConstraint, ExecutorState, RiskState, TrackStatus};
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrackRuntimeSnapshot {
    pub track_id: TrackId,
    pub instrument: Instrument,
    pub config: TrackConfig,
    pub status: TrackStatus,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    #[serde(default)]
    pub manual_target_override: Option<Exposure>,
    pub executor_state: ExecutorState,
    #[serde(default)]
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    #[serde(default)]
    pub ledger_state: TrackLedgerState,
    pub risk: RiskState,
    pub observed: ObservedState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedTrackState {
    pub snapshot: TrackRuntimeSnapshot,
    pub events: Vec<poise_core::events::DomainEvent>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct RiskStateCompat {
    #[serde(default)]
    realized_pnl_day: Option<NaiveDate>,
    #[serde(default)]
    realized_pnl_today: f64,
    #[serde(default)]
    realized_pnl_cumulative: f64,
    #[serde(default)]
    unrealized_pnl: f64,
    #[serde(default)]
    account_capacity_constraint: AccountCapacityConstraint,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct TrackRuntimeSnapshotCompat {
    track_id: TrackId,
    instrument: Instrument,
    config: TrackConfig,
    status: TrackStatus,
    current_exposure: Exposure,
    desired_exposure: Option<Exposure>,
    #[serde(default)]
    manual_target_override: Option<Exposure>,
    executor_state: ExecutorState,
    #[serde(default)]
    replacement_gate_reason: Option<ReplacementGateReason>,
    #[serde(default)]
    ledger_state: TrackLedgerState,
    risk: RiskStateCompat,
    observed: ObservedState,
}

impl From<TrackRuntimeSnapshotCompat> for TrackRuntimeSnapshot {
    fn from(value: TrackRuntimeSnapshotCompat) -> Self {
        let TrackRuntimeSnapshotCompat {
            track_id,
            instrument,
            config,
            status,
            current_exposure,
            desired_exposure,
            manual_target_override,
            executor_state,
            replacement_gate_reason,
            ledger_state,
            risk,
            observed,
        } = value;

        let ledger_state = if ledger_state.is_empty()
            && (risk.realized_pnl_day.is_some()
                || risk.realized_pnl_today.abs() > f64::EPSILON
                || risk.realized_pnl_cumulative.abs() > f64::EPSILON)
        {
            TrackLedgerState::from_legacy_realized(
                risk.realized_pnl_day,
                risk.realized_pnl_today,
                risk.realized_pnl_cumulative,
            )
        } else {
            ledger_state
        };

        Self {
            track_id,
            instrument,
            config,
            status,
            current_exposure,
            desired_exposure,
            manual_target_override,
            executor_state,
            replacement_gate_reason,
            ledger_state,
            risk: RiskState {
                unrealized_pnl: risk.unrealized_pnl,
                account_capacity_constraint: risk.account_capacity_constraint,
            },
            observed,
        }
    }
}

impl<'de> Deserialize<'de> for TrackRuntimeSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        TrackRuntimeSnapshotCompat::deserialize(deserializer).map(Into::into)
    }
}
