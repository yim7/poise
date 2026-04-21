use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::persisted_runtime::TrackRestoreRevision;
use poise_core::events::ReplacementGateReason;
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
    pub replacement_gate_reason: Option<ReplacementGateReason>,
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

#[cfg(test)]
mod tests {
    use crate::persisted_runtime::PersistedRuntimeCodec;
    use crate::runtime::{AutoState, ControlState, StrategyPriceStatus, TrackRuntime, TrackState};
    use crate::track::{Instrument, TrackId, Venue};
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::BandBoundary;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
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
                out_of_band_policy: BandProtectionPolicy::Freeze,
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
                out_of_band_policy: BandProtectionPolicy::Freeze,
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
                out_of_band_policy: BandProtectionPolicy::Freeze,
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
        runtime.track_state =
            TrackState::Running(ControlState::Manual(crate::runtime::ManualState::Flattened));

        let snapshot = runtime.snapshot();
        let json = PersistedRuntimeCodec::encode_snapshot(&snapshot).unwrap();
        let restored = PersistedRuntimeCodec::decode(json).unwrap();

        assert_eq!(
            serde_json::to_string(&restored.status()).unwrap(),
            "\"manual_flattening\""
        );
        assert_eq!(restored.manual_target_override(), Some(Exposure(0.0)));
    }

    #[test]
    fn snapshot_round_trips_flatten_pending_runtime_state() {
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
                out_of_band_policy: BandProtectionPolicy::Flatten {
                    trigger: poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps: 500 },
                    recover: poise_core::strategy::BandRecoverPolicy::ReentryConfirm { bps: 500 },
                },
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
        runtime.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(4.0),
                boundary: BandBoundary::Below,
            }));

        let json = PersistedRuntimeCodec::encode_snapshot(&runtime.snapshot()).unwrap();
        let restored = PersistedRuntimeCodec::decode(json).unwrap();

        assert_eq!(
            restored.runtime_state,
            TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(4.0),
                boundary: BandBoundary::Below,
            })),
        );
        assert_eq!(restored.status(), crate::runtime::TrackStatus::Frozen);
    }

    #[test]
    fn snapshot_round_trips_runtime_track_state() {
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
                out_of_band_policy: BandProtectionPolicy::Freeze,
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
        runtime.track_state = TrackState::Running(ControlState::Automatic(AutoState::Flattening {
            boundary: BandBoundary::Below,
        }));

        let snapshot = runtime.snapshot();
        let restored = PersistedRuntimeCodec::decode(
            PersistedRuntimeCodec::encode_snapshot(&snapshot).unwrap(),
        )
        .unwrap();

        assert_eq!(restored.runtime_state, snapshot.runtime_state);
    }
}
