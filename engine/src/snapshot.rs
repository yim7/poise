use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::persisted_runtime::TrackRestoreRevision;
use poise_core::events::ReplacementGateReason;
use poise_core::types::Exposure;

use crate::ledger::TrackLedgerState;
use crate::runtime::{ExecutorState, RiskState, TrackStatus};
use crate::track::TrackId;

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
    pub restore_revision: TrackRestoreRevision,
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

impl<'de> Deserialize<'de> for TrackRuntimeSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Snapshot {
            track_id: TrackId,
            restore_revision: TrackRestoreRevision,
            status: TrackStatus,
            current_exposure: Exposure,
            #[serde(default)]
            desired_exposure: Option<Exposure>,
            #[serde(default)]
            manual_target_override: Option<Exposure>,
            executor_state: ExecutorState,
            #[serde(default)]
            replacement_gate_reason: Option<ReplacementGateReason>,
            #[serde(default)]
            ledger_state: TrackLedgerState,
            risk: RiskState,
            #[serde(default)]
            observed: ObservedState,
        }

        let snapshot = Snapshot::deserialize(deserializer)?;
        Ok(Self {
            track_id: snapshot.track_id,
            restore_revision: snapshot.restore_revision,
            status: snapshot.status,
            current_exposure: snapshot.current_exposure,
            desired_exposure: snapshot.desired_exposure,
            manual_target_override: snapshot.manual_target_override,
            executor_state: snapshot.executor_state,
            replacement_gate_reason: snapshot.replacement_gate_reason,
            ledger_state: snapshot.ledger_state,
            risk: snapshot.risk,
            observed: snapshot.observed,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::persisted_runtime::PersistedRuntimeCodec;
    use crate::runtime::TrackRuntime;
    use crate::track::{Instrument, TrackId, Venue};
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::ExchangeRules;

    #[test]
    fn persisted_runtime_codec_encodes_runtime_only_snapshot() {
        let runtime = TrackRuntime::with_tick_timeout_secs(
            TrackId::new("track-1"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            CapacityBudget {
                max_notional: 6_000.0,
                daily_loss_limit: 500.0,
                total_loss_limit: 1_000.0,
            },
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.01,
                min_qty: 0.01,
                min_notional: 5.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
            chrono::Utc::now(),
            45,
        );

        let value = PersistedRuntimeCodec::encode_snapshot(&runtime.snapshot()).unwrap();

        assert!(value.get("instrument").is_none());
        assert!(value.get("config").is_none());
        assert!(value.get("restore_revision").is_some());
        assert_eq!(value["current_exposure"], serde_json::json!(0.0));
        assert_eq!(value["desired_exposure"], serde_json::Value::Null);
        assert_eq!(
            value["observed"]["reference_price"],
            serde_json::Value::Null
        );
        assert_eq!(value["risk"]["unrealized_pnl"], serde_json::json!(0.0));
        assert_eq!(value["track_id"], serde_json::json!("track-1"));
        assert_eq!(value["status"], serde_json::json!("waiting_market_data"));
        assert_eq!(value["manual_target_override"], serde_json::Value::Null);
        assert_eq!(
            value["ledger_state"]["gross_realized_pnl_cumulative"],
            serde_json::json!(0.0)
        );
        assert_eq!(
            value["executor_state"]["slots"][0]["working_order"],
            serde_json::Value::Null
        );
    }
}
