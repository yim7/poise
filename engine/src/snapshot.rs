use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::persisted_runtime::TrackRestoreRevision;
use crate::price_gate::PriceExecutionBlockReason;
use poise_core::events::ReplacementGateReason;
use poise_core::types::Exposure;

use crate::ledger::TrackLedgerState;
use crate::runtime::{ExecutorState, RiskState, StrategyPriceStatus, TrackStatus};
use crate::track::TrackId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ObservedState {
    pub strategy_price: Option<f64>,
    #[serde(default)]
    pub strategy_price_status: StrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
    #[serde(default)]
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
    #[serde(default)]
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
        assert_eq!(value["observed"]["strategy_price"], serde_json::Value::Null);
        assert_eq!(
            value["observed"]["strategy_price_status"],
            serde_json::json!("stale")
        );
        assert_eq!(value["observed"]["mark_price"], serde_json::Value::Null);
        assert_eq!(value["observed"]["best_bid"], serde_json::Value::Null);
        assert_eq!(value["observed"]["best_ask"], serde_json::Value::Null);
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
    fn snapshot_round_trips_strategy_price_mark_price_and_quote() {
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
        runtime.strategy_price = Some(95.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.mark_price = Some(95.2);
        runtime.best_bid = Some(94.9);
        runtime.best_ask = Some(95.1);

        let snapshot = runtime.snapshot();
        let json = PersistedRuntimeCodec::encode_snapshot(&snapshot).unwrap();

        assert!(json["observed"].get("reference_price").is_none());
        assert_eq!(json["observed"]["strategy_price"], serde_json::json!(95.0));
        assert_eq!(
            json["observed"]["strategy_price_status"],
            serde_json::json!("live")
        );
        assert_eq!(json["observed"]["mark_price"], serde_json::json!(95.2));
        assert_eq!(json["observed"]["best_bid"], serde_json::json!(94.9));
        assert_eq!(json["observed"]["best_ask"], serde_json::json!(95.1));

        let restored = PersistedRuntimeCodec::decode(json).unwrap();

        assert_eq!(restored.observed.strategy_price, Some(95.0));
        assert_eq!(
            restored.observed.strategy_price_status,
            StrategyPriceStatus::Live
        );
        assert_eq!(restored.observed.mark_price, Some(95.2));
        assert_eq!(restored.observed.best_bid, Some(94.9));
        assert_eq!(restored.observed.best_ask, Some(95.1));
    }
}
