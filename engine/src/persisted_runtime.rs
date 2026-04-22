use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use poise_core::events::ReplacementGateReason;
use poise_core::risk::CapacityBudget;
use poise_core::strategy::TrackConfig;
use poise_core::types::Exposure;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::execution_gate::ExecutionGateState;
use crate::ledger::TrackLedgerState;
use crate::runtime::{ExecutorState, RiskState, StrategyPriceStatus, TrackState};
use crate::snapshot::TrackRuntimeSnapshot;
use crate::track::{Instrument, TrackId};

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

    pub fn from_stored(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRuntimeSeed {
    pub track_id: TrackId,
    pub instrument: Instrument,
    pub track_config: TrackConfig,
    pub budget: CapacityBudget,
    pub tick_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostRestoreConstraints {
    pub budget: CapacityBudget,
    pub tick_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PersistedRuntimeRow {
    pub track_id: TrackId,
    pub restore_revision: Option<String>,
    pub runtime_state_json: String,
    pub current_exposure: f64,
    pub desired_exposure: Option<f64>,
    pub executor_state_json: Option<String>,
    pub replacement_gate_reason_json: Option<String>,
    pub execution_gate_state_json: Option<String>,
    pub ledger_state_json: Option<String>,
    pub unrealized_pnl: f64,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: String,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub out_of_band_since: Option<String>,
    pub last_tick_at: Option<String>,
    pub market_data_stale_since: Option<String>,
}

pub struct PersistedRuntimeCodec;

impl PersistedRuntimeCodec {
    pub fn encode_snapshot(snapshot: &TrackRuntimeSnapshot) -> Result<Value> {
        serde_json::to_value(snapshot).context("failed to serialize runtime-only snapshot")
    }

    pub fn decode(value: Value) -> Result<TrackRuntimeSnapshot> {
        serde_json::from_value(value).context("failed to deserialize persisted runtime")
    }

    pub fn decode_row(row: PersistedRuntimeRow) -> Result<TrackRuntimeSnapshot> {
        let runtime_state = serde_json::from_str::<TrackState>(&row.runtime_state_json)
            .context("failed to deserialize persisted runtime state")?;
        let executor_state = row
            .executor_state_json
            .as_deref()
            .map(serde_json::from_str::<ExecutorState>)
            .transpose()
            .context("failed to deserialize executor state")?;
        let replacement_gate_reason = row
            .replacement_gate_reason_json
            .as_deref()
            .map(serde_json::from_str::<ReplacementGateReason>)
            .transpose()
            .context("failed to deserialize replacement gate reason")?;
        let ledger_state = row
            .ledger_state_json
            .as_deref()
            .map(serde_json::from_str::<TrackLedgerState>)
            .transpose()
            .context("failed to deserialize ledger state")?;
        let execution_gate_state = row
            .execution_gate_state_json
            .as_deref()
            .map(serde_json::from_str::<ExecutionGateState>)
            .transpose()
            .context("failed to deserialize execution gate state")?;
        let out_of_band_since = row
            .out_of_band_since
            .as_deref()
            .map(Self::parse_timestamp)
            .transpose()?;
        let last_tick_at = row
            .last_tick_at
            .as_deref()
            .map(Self::parse_timestamp)
            .transpose()?;
        let market_data_stale_since = row
            .market_data_stale_since
            .as_deref()
            .map(Self::parse_timestamp)
            .transpose()?;
        let strategy_price_status = serde_json::from_str::<StrategyPriceStatus>(&format!(
            "\"{}\"",
            row.strategy_price_status
        ))
        .context("failed to deserialize strategy price status")?;

        let restore_revision = row
            .restore_revision
            .map(TrackRestoreRevision::from_stored)
            .ok_or_else(|| anyhow!("persisted runtime missing restore_revision"))?;
        let executor_state =
            executor_state.ok_or_else(|| anyhow!("persisted runtime missing executor_state"))?;

        Ok(TrackRuntimeSnapshot {
            track_id: row.track_id,
            restore_revision,
            runtime_state,
            current_exposure: Exposure(row.current_exposure),
            desired_exposure: row.desired_exposure.map(Exposure),
            executor_state,
            replacement_gate_reason,
            execution_gate_state: execution_gate_state.unwrap_or_else(ExecutionGateState::open),
            ledger_state: ledger_state.unwrap_or_default(),
            risk: RiskState {
                unrealized_pnl: row.unrealized_pnl,
            },
            observed: crate::snapshot::ObservedState {
                strategy_price: row.strategy_price,
                strategy_price_status,
                mark_price: row.mark_price,
                best_bid: row.best_bid,
                best_ask: row.best_ask,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
            },
        })
    }

    fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(value)
            .map(|parsed| parsed.with_timezone(&Utc))
            .context("failed to deserialize persisted timestamp")
    }
}
