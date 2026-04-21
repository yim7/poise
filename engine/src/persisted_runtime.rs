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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::ExchangeRules;
    use poise_core::types::Exposure;
    use serde_json::json;

    use crate::execution_gate::ExecutionGateState;
    use crate::runtime::{AutoState, ControlState, TrackRuntime, TrackState};
    use crate::track::{Instrument, TrackId, Venue};

    use super::{
        PersistedRuntimeCodec, PersistedRuntimeRow, PostRestoreConstraints, TrackRestoreRevision,
    };

    fn following_band_state_json() -> String {
        serde_json::to_string(&TrackState::Running(ControlState::Automatic(
            AutoState::FollowingBand,
        )))
        .unwrap()
    }

    fn flattening_state_json() -> String {
        serde_json::to_string(&TrackState::Running(ControlState::Automatic(
            AutoState::Flattening {
                boundary: poise_core::strategy::BandBoundary::Below,
            },
        )))
        .unwrap()
    }

    #[test]
    fn track_restore_revision_is_stable_for_same_instrument_and_track_config() {
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        let track_config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        };

        let left = TrackRestoreRevision::for_track(&instrument, &track_config);
        let right = TrackRestoreRevision::for_track(&instrument, &track_config);

        assert_eq!(left, right);
    }

    #[test]
    fn track_restore_revision_ignores_budget_and_tick_timeout_changes() {
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        let track_config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        };

        let revision = TrackRestoreRevision::for_track(&instrument, &track_config);
        let left = PostRestoreConstraints {
            budget: CapacityBudget {
                max_notional: 3000.0,
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
            },
            tick_timeout_secs: 30,
        };
        let right = PostRestoreConstraints {
            budget: CapacityBudget {
                max_notional: 4200.0,
                daily_loss_limit: 200.0,
                total_loss_limit: 800.0,
            },
            tick_timeout_secs: 45,
        };

        assert_eq!(
            revision,
            TrackRestoreRevision::for_track(&instrument, &track_config)
        );
        assert_ne!(left, right);
    }

    #[test]
    fn persisted_runtime_codec_rejects_legacy_json_snapshot_without_restore_revision() {
        let value = json!({
            "track_id": "btc-core",
            "instrument": { "venue": "binance", "symbol": "BTCUSDT" },
            "config": {
                "lower_price": 90.0,
                "upper_price": 110.0,
                "long_exposure_units": 8.0,
                "short_exposure_units": 8.0,
                "notional_per_unit": 375.0,
                "min_rebalance_units": 0.5,
                "shape_family": "linear",
                "out_of_band_policy": "freeze"
            },
            "status": "active",
            "current_exposure": 4.0,
            "desired_exposure": 6.0,
            "manual_target_override": null,
            "executor_state": {
                "active_round": null,
                "diagnostics": {
                    "mode": "passive",
                    "inventory_gap": 0.0,
                    "gap_started_at": null,
                    "last_reprice_at": null,
                    "last_execution_reason": null,
                    "recovery_anomaly": null
                },
                "slots": [{
                    "slot": "inventory_core",
                    "state": "empty",
                    "working_order": null
                }],
                "recent_terminal_orders": [],
                "stats": {
                    "started_at": "2026-03-29T09:00:00Z",
                    "max_inventory_gap_abs": 0.0,
                    "max_gap_age_ms": 0
                }
            },
            "replacement_gate_reason": null,
            "ledger_state": {
                "realized_pnl_day": null,
                "gross_realized_pnl_today": 0.0,
                "gross_realized_pnl_cumulative": 0.0,
                "trading_fee_today": 0.0,
                "trading_fee_cumulative": 0.0,
                "funding_fee_today": 0.0,
                "funding_fee_cumulative": 0.0,
                "unresolved_gaps": []
            },
            "risk": {
                "realized_pnl_day": "2026-03-29",
                "realized_pnl_today": -20.0,
                "realized_pnl_cumulative": -30.0,
                "unrealized_pnl": -5.0
            },
            "observed": {
                "strategy_price": 95.0,
                "strategy_price_status": "live",
                "out_of_band_since": null,
                "last_tick_at": null,
                "market_data_stale_since": null
            }
        });

        let error = PersistedRuntimeCodec::decode(value).expect_err("legacy snapshot should fail");
        let rendered = format!("{error:#}");

        assert!(
            rendered.contains("restore_revision"),
            "unexpected error: {rendered}"
        );
    }

    #[test]
    fn persisted_runtime_codec_reads_runtime_only_sqlite_row() {
        let snapshot = PersistedRuntimeCodec::decode_row(PersistedRuntimeRow {
            track_id: TrackId::new("btc-core"),
            restore_revision: Some(
                TrackRestoreRevision::for_track(
                    &Instrument::new(Venue::Binance, "BTCUSDT"),
                    &TrackConfig {
                        lower_price: 90.0,
                        upper_price: 110.0,
                        long_exposure_units: 8.0,
                        short_exposure_units: 8.0,
                        notional_per_unit: 375.0,
                        min_rebalance_units: 0.5,
                        shape_family: ShapeFamily::Linear,
                        out_of_band_policy: BandProtectionPolicy::Freeze,
                    },
                )
                .as_str()
                .to_string(),
            ),
            runtime_state_json: following_band_state_json(),
            current_exposure: 4.0,
            desired_exposure: Some(6.0),
            executor_state_json: Some(
                json!({
                    "active_round": null,
                    "diagnostics": {
                        "mode": "passive",
                        "inventory_gap": 0.0,
                        "gap_started_at": null,
                        "last_reprice_at": null,
                        "last_execution_reason": null,
                        "recovery_anomaly": null
                    },
                    "slots": [{
                        "slot": "inventory_core",
                        "state": "empty",
                        "working_order": null
                    }],
                    "recent_terminal_orders": [],
                    "stats": {
                        "started_at": "2026-03-29T09:00:00Z",
                        "max_inventory_gap_abs": 0.0,
                        "max_gap_age_ms": 0
                    }
                })
                .to_string(),
            ),
            replacement_gate_reason_json: None,
            execution_gate_state_json: Some(
                serde_json::to_string(&ExecutionGateState::open()).unwrap(),
            ),
            ledger_state_json: None,
            unrealized_pnl: -3.0,
            strategy_price: Some(95.0),
            strategy_price_status: "live".into(),
            mark_price: Some(95.2),
            best_bid: Some(94.9),
            best_ask: Some(95.1),
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
        })
        .unwrap();

        assert_eq!(snapshot.track_id.as_str(), "btc-core");
        assert_eq!(snapshot.restore_revision.as_str().len(), 64);
        assert!(snapshot.ledger_state.is_empty());
        assert_eq!(snapshot.current_exposure, Exposure(4.0));
    }

    #[test]
    fn persisted_runtime_restores_flattening_status() {
        let snapshot = PersistedRuntimeCodec::decode_row(PersistedRuntimeRow {
            track_id: TrackId::new("btc-core"),
            restore_revision: Some(
                TrackRestoreRevision::for_track(
                    &Instrument::new(Venue::Binance, "BTCUSDT"),
                    &TrackConfig {
                        lower_price: 90.0,
                        upper_price: 110.0,
                        long_exposure_units: 8.0,
                        short_exposure_units: 8.0,
                        notional_per_unit: 375.0,
                        min_rebalance_units: 0.5,
                        shape_family: ShapeFamily::Linear,
                        out_of_band_policy: BandProtectionPolicy::Freeze,
                    },
                )
                .as_str()
                .to_string(),
            ),
            runtime_state_json: flattening_state_json(),
            current_exposure: 0.0,
            desired_exposure: Some(0.0),
            executor_state_json: Some(
                json!({
                    "active_round": null,
                    "diagnostics": {
                        "mode": "passive",
                        "inventory_gap": 0.0,
                        "gap_started_at": null,
                        "last_reprice_at": null,
                        "last_execution_reason": null,
                        "recovery_anomaly": null
                    },
                    "slots": [{
                        "slot": "inventory_core",
                        "state": "empty",
                        "working_order": null
                    }],
                    "recent_terminal_orders": [],
                    "stats": {
                        "started_at": "2026-03-29T09:00:00Z",
                        "max_inventory_gap_abs": 0.0,
                        "max_gap_age_ms": 0
                    }
                })
                .to_string(),
            ),
            replacement_gate_reason_json: None,
            execution_gate_state_json: Some(
                serde_json::to_string(&ExecutionGateState::open()).unwrap(),
            ),
            ledger_state_json: None,
            unrealized_pnl: 0.0,
            strategy_price: Some(95.0),
            strategy_price_status: "live".into(),
            mark_price: Some(95.2),
            best_bid: Some(94.9),
            best_ask: Some(95.1),
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
        })
        .unwrap();

        assert_eq!(
            serde_json::to_string(&snapshot.status()).unwrap(),
            "\"flattening\""
        );
    }

    #[test]
    fn persisted_runtime_codec_rejects_runtime_only_row_without_restore_revision() {
        let error = PersistedRuntimeCodec::decode_row(PersistedRuntimeRow {
            track_id: TrackId::new("btc-core"),
            restore_revision: None,
            runtime_state_json: following_band_state_json(),
            current_exposure: 4.0,
            desired_exposure: Some(6.0),
            executor_state_json: Some(
                json!({
                    "active_round": null,
                    "diagnostics": {
                        "mode": "passive",
                        "inventory_gap": 0.0,
                        "gap_started_at": null,
                        "last_reprice_at": null,
                        "last_execution_reason": null,
                        "recovery_anomaly": null
                    },
                    "slots": [{
                        "slot": "inventory_core",
                        "state": "empty",
                        "working_order": null
                    }],
                    "recent_terminal_orders": [],
                    "stats": {
                        "started_at": "2026-03-29T09:00:00Z",
                        "max_inventory_gap_abs": 0.0,
                        "max_gap_age_ms": 0
                    }
                })
                .to_string(),
            ),
            replacement_gate_reason_json: None,
            execution_gate_state_json: Some(
                serde_json::to_string(&ExecutionGateState::open()).unwrap(),
            ),
            ledger_state_json: None,
            unrealized_pnl: -3.0,
            strategy_price: Some(95.0),
            strategy_price_status: "live".into(),
            mark_price: Some(95.2),
            best_bid: Some(94.9),
            best_ask: Some(95.1),
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
        })
        .expect_err("missing restore_revision should fail");

        assert!(
            error.to_string().contains("restore_revision"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn encode_snapshot_keeps_runtime_only_artifact() {
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
            Utc::now(),
            45,
        );

        let value = PersistedRuntimeCodec::encode_snapshot(&runtime.snapshot()).unwrap();

        assert!(value.get("instrument").is_none());
        assert!(value.get("config").is_none());
        assert_eq!(value["track_id"], json!("track-1"));
        assert!(value.get("restore_revision").is_some());
    }
}
