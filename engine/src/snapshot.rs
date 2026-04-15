use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::persisted_runtime::TrackRestoreRevision;
use poise_core::events::ReplacementGateReason;
use poise_core::types::Exposure;

use crate::ledger::TrackLedgerState;
use crate::price_gate::PriceExecutionBlockReason;
use crate::runtime::{ExecutorState, RiskState, StrategyPriceStatus, TrackStatus};
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
    pub status: TrackStatus,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    #[serde(default)]
    pub manual_target_override: Option<Exposure>,
    pub executor_state: ExecutorState,
    #[serde(default)]
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_execution_block_reason: Option<PriceExecutionBlockReason>,
    pub ledger_state: TrackLedgerState,
    pub risk: RiskState,
    pub observed: ObservedState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedTrackState {
    pub snapshot: TrackRuntimeSnapshot,
    pub events: Vec<poise_core::events::DomainEvent>,
}

#[cfg(test)]
mod tests {
    use crate::persisted_runtime::PersistedRuntimeCodec;
    use crate::runtime::{StrategyPriceStatus, TrackRuntime};
    use crate::track::{Instrument, TrackId, Venue};
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure};

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

    #[test]
    fn persisted_snapshot_preserves_durable_desired_exposure_but_omits_raw_live_target() {
        let mut runtime = TrackRuntime::with_tick_timeout_secs(
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
        runtime.desired_exposure = Some(Exposure(6.0));
        runtime.strategy_price = Some(95.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.mark_price = Some(95.2);
        runtime.best_bid = Some(94.9);
        runtime.best_ask = Some(95.1);

        let value = PersistedRuntimeCodec::encode_snapshot(&runtime.snapshot()).unwrap();

        assert_eq!(value["desired_exposure"], serde_json::json!(6.0));
        assert!(value["observed"].get("strategy_price").is_none());
        assert!(value["observed"].get("strategy_price_status").is_none());
        assert!(value["observed"].get("mark_price").is_none());
        assert!(value["observed"].get("best_bid").is_none());
        assert!(value["observed"].get("best_ask").is_none());
        assert!(value["observed"].get("last_tick_at").is_none());
    }

    #[test]
    fn snapshot_round_trips_manual_flattening_status() {
        let mut runtime = TrackRuntime::with_tick_timeout_secs(
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
        runtime.status = serde_json::from_str("\"manual_flattening\"").unwrap();
        runtime.manual_target_override = Some(Exposure(0.0));

        let snapshot = runtime.snapshot();
        let json = PersistedRuntimeCodec::encode_snapshot(&snapshot).unwrap();
        let restored = PersistedRuntimeCodec::decode(json).unwrap();

        assert_eq!(
            serde_json::to_string(&restored.status).unwrap(),
            "\"manual_flattening\""
        );
        assert_eq!(restored.manual_target_override, Some(Exposure(0.0)));
    }
}
