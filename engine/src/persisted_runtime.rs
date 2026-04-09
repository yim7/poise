use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, NaiveDate, Utc};
use poise_core::events::ReplacementGateReason;
use poise_core::risk::CapacityBudget;
use poise_core::strategy::{TrackConfig, validate_config};
use poise_core::types::Exposure;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ledger::{LegacyRealizedState, TrackLedgerState};
use crate::runtime::{AccountCapacityConstraint, ExecutorState, RiskState, TrackStatus};
use crate::snapshot::{ObservedState, TrackRuntimeSnapshot};
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
    pub venue: Option<String>,
    pub symbol: Option<String>,
    pub config_json: Option<String>,
    pub status_json: String,
    pub current_exposure: f64,
    pub desired_exposure: Option<f64>,
    pub manual_target_override: Option<f64>,
    pub executor_state_json: Option<String>,
    pub replacement_gate_reason_json: Option<String>,
    pub ledger_state_json: Option<String>,
    pub realized_pnl_day: Option<String>,
    pub realized_pnl_today: f64,
    pub realized_pnl_cumulative: f64,
    pub unrealized_pnl: f64,
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<String>,
    pub last_tick_at: Option<String>,
    pub market_data_stale_since: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PersistedRiskStateCompat {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PersistedRuntimeSnapshotCompat {
    track_id: TrackId,
    #[serde(default)]
    restore_revision: Option<TrackRestoreRevision>,
    #[serde(default)]
    instrument: Option<Instrument>,
    #[serde(default)]
    config: Option<TrackConfig>,
    status: TrackStatus,
    current_exposure: Exposure,
    #[serde(default)]
    desired_exposure: Option<Exposure>,
    #[serde(default)]
    manual_target_override: Option<Exposure>,
    #[serde(default)]
    executor_state: Option<ExecutorState>,
    #[serde(default)]
    replacement_gate_reason: Option<ReplacementGateReason>,
    #[serde(default)]
    ledger_state: Option<TrackLedgerState>,
    risk: PersistedRiskStateCompat,
    #[serde(default)]
    observed: ObservedState,
}

pub struct PersistedRuntimeCodec;

impl PersistedRuntimeCodec {
    pub fn encode_snapshot(snapshot: &TrackRuntimeSnapshot) -> Result<Value> {
        serde_json::to_value(snapshot).context("failed to serialize runtime-only snapshot")
    }

    pub fn decode(value: Value) -> Result<TrackRuntimeSnapshot> {
        let snapshot: PersistedRuntimeSnapshotCompat =
            serde_json::from_value(value).context("failed to deserialize persisted runtime")?;
        Self::from_compat(snapshot)
    }

    pub fn decode_row(row: PersistedRuntimeRow) -> Result<TrackRuntimeSnapshot> {
        let instrument = match (row.venue, row.symbol) {
            (Some(venue), Some(symbol)) => {
                Some(Instrument::new(Self::parse_venue(&venue)?, symbol))
            }
            (None, None) => None,
            _ => {
                return Err(anyhow!(
                    "persisted runtime row `{}` has partial legacy instrument columns",
                    row.track_id.as_str()
                ));
            }
        };
        let track_config = row
            .config_json
            .as_deref()
            .map(Self::parse_track_config)
            .transpose()?;
        let status = serde_json::from_str::<TrackStatus>(&row.status_json)
            .context("failed to deserialize persisted track status")?;
        let executor_state = row
            .executor_state_json
            .as_deref()
            .map(|json| serde_json::from_str::<ExecutorState>(json))
            .transpose()
            .context("failed to deserialize executor state")?;
        let replacement_gate_reason = row
            .replacement_gate_reason_json
            .as_deref()
            .map(|json| serde_json::from_str::<ReplacementGateReason>(json))
            .transpose()
            .context("failed to deserialize replacement gate reason")?;
        let ledger_state = row
            .ledger_state_json
            .as_deref()
            .map(|json| serde_json::from_str::<TrackLedgerState>(json))
            .transpose()
            .context("failed to deserialize ledger state")?;
        let realized_pnl_day = row
            .realized_pnl_day
            .as_deref()
            .map(|value| NaiveDate::parse_from_str(value, "%F"))
            .transpose()
            .context("failed to deserialize realized_pnl_day")?;
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

        Self::from_compat(PersistedRuntimeSnapshotCompat {
            track_id: row.track_id,
            restore_revision: row.restore_revision.map(TrackRestoreRevision::from_stored),
            instrument,
            config: track_config,
            status,
            current_exposure: Exposure(row.current_exposure),
            desired_exposure: row.desired_exposure.map(Exposure),
            manual_target_override: row.manual_target_override.map(Exposure),
            executor_state,
            replacement_gate_reason,
            ledger_state,
            risk: PersistedRiskStateCompat {
                realized_pnl_day,
                realized_pnl_today: row.realized_pnl_today,
                realized_pnl_cumulative: row.realized_pnl_cumulative,
                unrealized_pnl: row.unrealized_pnl,
                account_capacity_constraint: AccountCapacityConstraint::default(),
            },
            observed: ObservedState {
                reference_price: row.reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
            },
        })
    }

    fn from_compat(snapshot: PersistedRuntimeSnapshotCompat) -> Result<TrackRuntimeSnapshot> {
        let restore_revision = match snapshot.restore_revision {
            Some(restore_revision) => restore_revision,
            None => {
                let instrument = snapshot
                    .instrument
                    .as_ref()
                    .ok_or_else(|| anyhow!("legacy persisted runtime missing instrument"))?;
                let track_config = snapshot
                    .config
                    .as_ref()
                    .ok_or_else(|| anyhow!("legacy persisted runtime missing track config"))?;
                TrackRestoreRevision::for_track(instrument, track_config)
            }
        };
        let executor_state = snapshot
            .executor_state
            .ok_or_else(|| anyhow!("persisted runtime missing executor_state"))?;

        Ok(TrackRuntimeSnapshot {
            track_id: snapshot.track_id,
            restore_revision,
            status: snapshot.status,
            current_exposure: snapshot.current_exposure,
            desired_exposure: snapshot.desired_exposure,
            manual_target_override: snapshot.manual_target_override,
            executor_state,
            replacement_gate_reason: snapshot.replacement_gate_reason,
            ledger_state: TrackLedgerState::from_persisted(
                snapshot.ledger_state,
                LegacyRealizedState {
                    realized_pnl_day: snapshot.risk.realized_pnl_day,
                    gross_realized_pnl_today: snapshot.risk.realized_pnl_today,
                    gross_realized_pnl_cumulative: snapshot.risk.realized_pnl_cumulative,
                },
            ),
            risk: RiskState {
                unrealized_pnl: snapshot.risk.unrealized_pnl,
                account_capacity_constraint: snapshot.risk.account_capacity_constraint,
            },
            observed: snapshot.observed,
        })
    }

    fn parse_track_config(json: &str) -> Result<TrackConfig> {
        let config: TrackConfig =
            serde_json::from_str(json).context("failed to deserialize legacy track config")?;
        validate_config(&config).map_err(|error| anyhow!(error))?;
        Ok(config)
    }

    fn parse_venue(venue: &str) -> Result<crate::track::Venue> {
        match venue {
            "binance" => Ok(crate::track::Venue::Binance),
            other => Err(anyhow!("unknown venue `{other}`")),
        }
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
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::ExchangeRules;
    use poise_core::types::Exposure;
    use serde_json::json;

    use crate::runtime::TrackRuntime;
    use crate::track::{Instrument, TrackId, Venue};

    use super::{
        PersistedRuntimeCodec, PersistedRuntimeRow, PostRestoreConstraints, TrackRestoreRevision,
    };

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
            out_of_band_policy: OutOfBandPolicy::Freeze,
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
            out_of_band_policy: OutOfBandPolicy::Freeze,
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
    fn persisted_runtime_codec_backfills_restore_revision_from_legacy_json_snapshot() {
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
                "unrealized_pnl": -5.0,
                "account_capacity_constraint": {
                    "increase_blocked": false,
                    "blocked_reason": null,
                    "max_increase_notional": null
                }
            },
            "observed": {
                "reference_price": 95.0,
                "out_of_band_since": null,
                "last_tick_at": null,
                "market_data_stale_since": null
            }
        });

        let snapshot = PersistedRuntimeCodec::decode(value).unwrap();

        assert_eq!(snapshot.track_id.as_str(), "btc-core");
        assert_eq!(snapshot.restore_revision.as_str().len(), 64);
        assert_eq!(snapshot.ledger_state.net_realized_pnl_today(), -20.0);
        assert_eq!(snapshot.risk.unrealized_pnl, -5.0);
    }

    #[test]
    fn persisted_runtime_codec_reads_legacy_sqlite_row() {
        let snapshot = PersistedRuntimeCodec::decode_row(PersistedRuntimeRow {
            track_id: TrackId::new("btc-core"),
            restore_revision: None,
            venue: Some("binance".into()),
            symbol: Some("BTCUSDT".into()),
            config_json: Some(
                json!({
                    "lower_price": 90.0,
                    "upper_price": 110.0,
                    "long_exposure_units": 8.0,
                    "short_exposure_units": 8.0,
                    "notional_per_unit": 375.0,
                    "min_rebalance_units": 0.5,
                    "shape_family": "linear",
                    "out_of_band_policy": "freeze"
                })
                .to_string(),
            ),
            status_json: "\"active\"".into(),
            current_exposure: 4.0,
            desired_exposure: Some(6.0),
            manual_target_override: None,
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
            ledger_state_json: None,
            realized_pnl_day: Some("2026-03-29".into()),
            realized_pnl_today: -12.0,
            realized_pnl_cumulative: -18.0,
            unrealized_pnl: -3.0,
            reference_price: Some(95.0),
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
        })
        .unwrap();

        assert_eq!(snapshot.track_id.as_str(), "btc-core");
        assert_eq!(snapshot.restore_revision.as_str().len(), 64);
        assert_eq!(snapshot.ledger_state.net_realized_pnl_cumulative(), -18.0);
        assert_eq!(snapshot.current_exposure, Exposure(4.0));
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
