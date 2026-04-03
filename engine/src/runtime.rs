use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use poise_core::events::ReplacementGateReason;
use poise_core::risk::CapacityBudget;
use poise_core::strategy::TrackConfig;
use poise_core::types::{ExchangeRules, Exposure, Side};

use crate::executor::{
    ExecutionMode, ExecutionReason, INVENTORY_CORE_SLOT, OrderRole, OrderSlot, RecoveryAnomaly,
};
use crate::ports::OrderStatus;
use crate::snapshot::{ObservedState, TrackRuntimeSnapshot};
use crate::track::{Instrument, TrackId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AccountCapacityConstraint {
    pub increase_blocked: bool,
    pub blocked_reason: Option<String>,
    pub max_increase_notional: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub realized_pnl_today: f64,
    pub realized_pnl_cumulative: f64,
    pub unrealized_pnl: f64,
    #[serde(default)]
    pub account_capacity_constraint: AccountCapacityConstraint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionStats {
    pub started_at: DateTime<Utc>,
    pub max_inventory_gap_abs: Exposure,
    pub max_gap_age_ms: i64,
}

impl ExecutionStats {
    pub fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at,
            max_inventory_gap_abs: Exposure(0.0),
            max_gap_age_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionRound {
    pub target_exposure: Exposure,
    pub mode: ExecutionMode,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutorDiagnostics {
    pub mode: ExecutionMode,
    pub inventory_gap: Exposure,
    pub gap_started_at: Option<DateTime<Utc>>,
    pub last_reprice_at: Option<DateTime<Utc>>,
    pub last_execution_reason: Option<ExecutionReason>,
    #[serde(default)]
    pub recovery_anomaly: Option<RecoveryAnomaly>,
}

impl ExecutorDiagnostics {
    pub fn empty() -> Self {
        Self {
            mode: ExecutionMode::Passive,
            inventory_gap: Exposure(0.0),
            gap_started_at: None,
            last_reprice_at: None,
            last_execution_reason: None,
            recovery_anomaly: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotState {
    Empty,
    SubmitPending,
    Working,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkingOrder {
    pub order_id: Option<String>,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub status: OrderStatus,
    pub role: OrderRole,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionSlot {
    // Invariant: one slot owns at most one working order.
    pub slot: OrderSlot,
    pub state: SlotState,
    pub working_order: Option<WorkingOrder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentTerminalOrder {
    pub client_order_id: String,
    pub order_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutorState {
    pub active_round: Option<ExecutionRound>,
    pub diagnostics: ExecutorDiagnostics,
    pub slots: Vec<ExecutionSlot>,
    pub recent_terminal_orders: Vec<RecentTerminalOrder>,
    pub stats: ExecutionStats,
}

impl ExecutorState {
    pub fn empty(started_at: DateTime<Utc>) -> Self {
        Self {
            active_round: None,
            diagnostics: ExecutorDiagnostics::empty(),
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new(INVENTORY_CORE_SLOT),
                state: SlotState::Empty,
                working_order: None,
            }],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats::new(started_at),
        }
    }

    pub fn reset_for_activation(&self, started_at: DateTime<Utc>) -> Self {
        let mut reset = self.clone();
        reset.diagnostics.gap_started_at =
            (!reset.diagnostics.inventory_gap.is_zero()).then_some(started_at);
        reset.diagnostics.last_reprice_at = None;
        reset.diagnostics.last_execution_reason = None;
        reset.diagnostics.recovery_anomaly = None;
        reset.stats = ExecutionStats::new(started_at);
        reset
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ExecutorStateCompat {
    #[serde(default)]
    active_round: Option<ExecutionRound>,
    #[serde(default)]
    diagnostics: Option<ExecutorDiagnostics>,
    #[serde(default)]
    mode: Option<ExecutionMode>,
    #[serde(default)]
    inventory_gap: Option<Exposure>,
    #[serde(default)]
    gap_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_reprice_at: Option<DateTime<Utc>>,
    #[serde(default)]
    slots: Vec<ExecutionSlot>,
    #[serde(default)]
    recent_terminal_orders: Vec<RecentTerminalOrder>,
    #[serde(default)]
    last_execution_reason: Option<ExecutionReason>,
    #[serde(default)]
    recovery_anomaly: Option<RecoveryAnomaly>,
    stats: ExecutionStats,
}

#[derive(Serialize)]
struct ExecutorStateSerialized<'a> {
    active_round: &'a Option<ExecutionRound>,
    diagnostics: &'a ExecutorDiagnostics,
    slots: &'a [ExecutionSlot],
    recent_terminal_orders: &'a [RecentTerminalOrder],
    stats: &'a ExecutionStats,
}

impl From<ExecutorStateCompat> for ExecutorState {
    fn from(value: ExecutorStateCompat) -> Self {
        let diagnostics = value.diagnostics.unwrap_or(ExecutorDiagnostics {
            mode: value.mode.unwrap_or(ExecutionMode::Passive),
            inventory_gap: value.inventory_gap.unwrap_or(Exposure(0.0)),
            gap_started_at: value.gap_started_at,
            last_reprice_at: value.last_reprice_at,
            last_execution_reason: value.last_execution_reason,
            recovery_anomaly: value.recovery_anomaly,
        });

        Self {
            active_round: value.active_round,
            diagnostics,
            slots: if value.slots.is_empty() {
                vec![ExecutionSlot {
                    slot: OrderSlot::new(INVENTORY_CORE_SLOT),
                    state: SlotState::Empty,
                    working_order: None,
                }]
            } else {
                value.slots
            },
            recent_terminal_orders: value.recent_terminal_orders,
            stats: value.stats,
        }
    }
}

impl Serialize for ExecutorState {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ExecutorStateSerialized {
            active_round: &self.active_round,
            diagnostics: &self.diagnostics,
            slots: &self.slots,
            recent_terminal_orders: &self.recent_terminal_orders,
            stats: &self.stats,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ExecutorState {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ExecutorStateCompat::deserialize(deserializer).map(Into::into)
    }
}

#[derive(Debug, Clone)]
pub struct TrackRuntime {
    pub(crate) id: TrackId,
    pub(crate) instrument: Instrument,
    pub(crate) config: TrackConfig,
    pub(crate) budget: CapacityBudget,
    pub(crate) exchange_rules: ExchangeRules,
    pub(crate) status: TrackStatus,
    pub(crate) current_exposure: Exposure,
    // Reconcile owns desired_exposure; exchange sync/restore own observed order and risk fields.
    pub(crate) desired_exposure: Option<Exposure>,
    pub(crate) manual_target_override: Option<Exposure>,
    pub(crate) executor_state: ExecutorState,
    pub(crate) replacement_gate_reason: Option<ReplacementGateReason>,
    pub(crate) risk_state: RiskState,
    pub(crate) reference_price: Option<f64>,
    pub(crate) out_of_band_since: Option<DateTime<Utc>>,
    pub(crate) last_tick_at: Option<DateTime<Utc>>,
    pub(crate) market_data_stale_since: Option<DateTime<Utc>>,
    pub(crate) tick_timeout_secs: u64,
}

impl TrackRuntime {
    pub fn new(
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        budget: CapacityBudget,
        exchange_rules: ExchangeRules,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self::with_tick_timeout_secs(
            id,
            instrument,
            config,
            budget,
            exchange_rules,
            started_at,
            30,
        )
    }

    pub fn with_tick_timeout_secs(
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        budget: CapacityBudget,
        exchange_rules: ExchangeRules,
        started_at: DateTime<Utc>,
        tick_timeout_secs: u64,
    ) -> Self {
        Self {
            id,
            instrument,
            config,
            budget,
            exchange_rules,
            status: TrackStatus::WaitingMarketData,
            current_exposure: Exposure(0.0),
            desired_exposure: None,
            manual_target_override: None,
            executor_state: ExecutorState::empty(started_at),
            replacement_gate_reason: None,
            risk_state: RiskState::default(),
            reference_price: None,
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
            tick_timeout_secs,
        }
    }

    pub fn symbol(&self) -> &str {
        &self.instrument.symbol
    }

    pub fn id(&self) -> &TrackId {
        &self.id
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn status(&self) -> &TrackStatus {
        &self.status
    }

    pub fn budget(&self) -> &CapacityBudget {
        &self.budget
    }

    pub fn snapshot(&self) -> TrackRuntimeSnapshot {
        TrackRuntimeSnapshot {
            track_id: self.id.clone(),
            instrument: self.instrument.clone(),
            config: self.config.clone(),
            status: self.status.clone(),
            current_exposure: self.current_exposure.clone(),
            desired_exposure: self.desired_exposure.clone(),
            manual_target_override: self.manual_target_override.clone(),
            executor_state: self.executor_state.clone(),
            replacement_gate_reason: self.replacement_gate_reason.clone(),
            risk: self.risk_state.clone(),
            observed: ObservedState {
                reference_price: self.reference_price,
                out_of_band_since: self.out_of_band_since,
                last_tick_at: self.last_tick_at,
                market_data_stale_since: self.market_data_stale_since,
            },
        }
    }

    pub fn restore_from_snapshot(&mut self, snapshot: &TrackRuntimeSnapshot) -> Result<()> {
        if self.id != snapshot.track_id {
            anyhow::bail!(
                "snapshot track id mismatch: runtime has `{}`, snapshot has `{}`",
                self.id.as_str(),
                snapshot.track_id.as_str()
            );
        }
        if self.instrument != snapshot.instrument {
            anyhow::bail!(
                "snapshot instrument mismatch for `{}`: expected `{}:{}`, got `{}:{}`",
                self.id.as_str(),
                self.instrument.venue.as_str(),
                self.instrument.symbol,
                snapshot.instrument.venue.as_str(),
                snapshot.instrument.symbol
            );
        }
        if self.config != snapshot.config {
            anyhow::bail!("snapshot config mismatch for `{}`", self.id.as_str());
        }

        self.status = snapshot.status.clone();
        self.current_exposure = snapshot.current_exposure.clone();
        self.desired_exposure = snapshot.desired_exposure.clone();
        self.manual_target_override = snapshot.manual_target_override.clone();
        self.executor_state = snapshot.executor_state.clone();
        self.replacement_gate_reason = snapshot.replacement_gate_reason.clone();
        self.risk_state = snapshot.risk.clone();
        self.reference_price = snapshot.observed.reference_price;
        self.out_of_band_since = snapshot.observed.out_of_band_since;
        self.last_tick_at = snapshot.observed.last_tick_at;
        self.market_data_stale_since = snapshot.observed.market_data_stale_since;

        debug_assert_eq!(
            self.snapshot(),
            *snapshot,
            "restore_from_snapshot left persisted fields unsynced"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use chrono::{DateTime, TimeZone, Utc};
    use poise_core::events::ReplacementGateReason;
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};

    use crate::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot};
    use crate::ports::OrderStatus;
    use crate::snapshot::TrackRuntimeSnapshot;
    use crate::track::{Instrument, TrackId, Venue};

    use super::{
        AccountCapacityConstraint, ExecutionRound, ExecutionSlot, ExecutionStats,
        ExecutorDiagnostics, ExecutorState, RiskState, SlotState, TrackRuntime, TrackStatus,
        WorkingOrder,
    };

    fn test_runtime() -> TrackRuntime {
        TrackRuntime::new(
            TrackId::new("grid-1"),
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
                daily_loss_limit: -500.0,
                stop_loss_pct: 10.0,
            },
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.01,
                min_qty: 0.01,
                min_notional: 5.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        )
    }

    fn test_executor_state() -> ExecutorState {
        let started_at = DateTime::parse_from_rfc3339("2026-03-29T07:55:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let slot = ExecutionSlot {
            slot: OrderSlot::new("passive_buy_1"),
            state: SlotState::Working,
            working_order: Some(WorkingOrder {
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                status: OrderStatus::New,
                role: OrderRole::IncreaseInventory,
            }),
        };

        ExecutorState {
            active_round: Some(ExecutionRound {
                target_exposure: Exposure(6.0),
                mode: ExecutionMode::Passive,
                started_at,
            }),
            diagnostics: ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(2.0),
                gap_started_at: Some(
                    DateTime::parse_from_rfc3339("2026-03-29T08:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                last_reprice_at: Some(
                    DateTime::parse_from_rfc3339("2026-03-29T08:01:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
            },
            slots: vec![slot],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats {
                started_at,
                max_inventory_gap_abs: Exposure(3.0),
                max_gap_age_ms: 42_000,
            },
        }
    }

    #[test]
    fn margin_guard_snapshot_round_trips_executor_state() {
        let mut runtime = test_runtime();
        runtime.status = TrackStatus::Active;
        runtime.current_exposure = Exposure(4.0);
        runtime.desired_exposure = Some(Exposure(6.0));
        runtime.manual_target_override = Some(Exposure(0.0));
        runtime.last_tick_at = Some(
            DateTime::parse_from_rfc3339("2026-03-29T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        runtime.market_data_stale_since = Some(
            DateTime::parse_from_rfc3339("2026-03-29T08:01:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        runtime.tick_timeout_secs = 45;
        runtime.replacement_gate_reason = Some(ReplacementGateReason::RoundedMatch);
        runtime.risk_state = RiskState {
            realized_pnl_day: None,
            realized_pnl_today: 1.0,
            realized_pnl_cumulative: 2.0,
            unrealized_pnl: -0.5,
            account_capacity_constraint: AccountCapacityConstraint {
                increase_blocked: true,
                blocked_reason: Some("insufficient_margin".into()),
                max_increase_notional: Some(1_500.0),
            },
        };
        runtime.reference_price = Some(96.0);
        runtime.out_of_band_since = Some(
            DateTime::parse_from_rfc3339("2026-03-29T08:02:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        runtime.executor_state = test_executor_state();

        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.executor_state, runtime.executor_state);

        let serialized = serde_json::to_value(&snapshot).unwrap();
        assert!(serialized.get("executor_state").is_some());
        assert!(serialized.get("pending_order").is_none());
        assert!(
            serialized
                .get("executor_state")
                .unwrap()
                .get("desired_orders")
                .is_none()
        );

        let mut restored = test_runtime();
        restored.tick_timeout_secs = 45;
        restored.restore_from_snapshot(&snapshot).unwrap();

        assert_eq!(restored.status, TrackStatus::Active);
        assert_eq!(restored.current_exposure, Exposure(4.0));
        assert_eq!(restored.desired_exposure, Some(Exposure(6.0)));
        assert_eq!(restored.manual_target_override, Some(Exposure(0.0)));
        assert_eq!(restored.last_tick_at, runtime.last_tick_at);
        assert_eq!(
            restored.market_data_stale_since,
            runtime.market_data_stale_since
        );
        assert_eq!(restored.tick_timeout_secs, 45);
        assert_eq!(restored.executor_state, runtime.executor_state);
        assert_eq!(
            restored.executor_state.stats.started_at,
            runtime.executor_state.stats.started_at
        );
        assert_eq!(
            restored.risk_state.account_capacity_constraint,
            runtime.risk_state.account_capacity_constraint
        );
    }

    #[test]
    fn empty_executor_state_has_no_active_round_and_empty_inventory_core_slot() {
        let state = ExecutorState::empty(Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap());
        let json = serde_json::to_value(&state).unwrap();
        let object = json
            .as_object()
            .expect("executor state should serialize as an object");

        assert!(
            object.contains_key("active_round"),
            "executor state should carry active_round explicitly"
        );
        assert_eq!(object.get("active_round"), Some(&serde_json::Value::Null));
        assert!(
            object.contains_key("diagnostics"),
            "executor diagnostics should be nested under diagnostics"
        );
        assert!(!object.contains_key("mode"));
        assert_eq!(json["slots"][0]["slot"], json!("inventory_core"));
        assert_eq!(json["slots"][0]["state"], json!("empty"));
    }

    #[test]
    fn snapshot_round_trips_active_round_and_diagnostics() {
        let mut runtime = test_runtime();
        runtime.status = TrackStatus::Active;
        runtime.current_exposure = Exposure(4.0);
        runtime.desired_exposure = Some(Exposure(6.0));
        runtime.executor_state = test_executor_state();

        let snapshot = runtime.snapshot();
        let json = serde_json::to_value(&snapshot).unwrap();
        let executor = json["executor_state"]
            .as_object()
            .expect("executor state should serialize as an object");

        assert!(
            executor.contains_key("active_round"),
            "snapshot should persist active_round"
        );
        assert!(
            executor.contains_key("diagnostics"),
            "snapshot should persist diagnostics as a nested object"
        );
        assert!(!executor.contains_key("mode"));
        assert!(!executor.contains_key("inventory_gap"));

        let restored: TrackRuntimeSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(restored.executor_state, snapshot.executor_state);
    }

    #[test]
    fn restore_from_snapshot_detects_missing_field_via_round_trip() {
        let mut runtime = test_runtime();
        runtime.status = TrackStatus::Active;
        runtime.current_exposure = Exposure(4.0);
        runtime.desired_exposure = Some(Exposure(6.0));
        runtime.reference_price = Some(96.0);
        runtime.executor_state = test_executor_state();

        let snapshot = runtime.snapshot();
        let mut fresh = test_runtime();
        fresh.restore_from_snapshot(&snapshot).unwrap();

        assert_eq!(fresh.snapshot(), snapshot);
    }

    #[test]
    fn margin_guard_snapshot_round_trips_account_capacity_constraint_json() {
        let mut runtime = test_runtime();
        runtime.risk_state.account_capacity_constraint = AccountCapacityConstraint {
            increase_blocked: true,
            blocked_reason: Some("insufficient_margin".into()),
            max_increase_notional: Some(900.0),
        };

        let snapshot = runtime.snapshot();
        let serialized = serde_json::to_value(&snapshot).unwrap();

        assert_eq!(
            serialized["risk"]["account_capacity_constraint"],
            json!({
                "increase_blocked": true,
                "blocked_reason": "insufficient_margin",
                "max_increase_notional": 900.0
            })
        );

        let restored: TrackRuntimeSnapshot = serde_json::from_value(serialized).unwrap();
        assert_eq!(
            restored.risk.account_capacity_constraint,
            snapshot.risk.account_capacity_constraint
        );
    }

    #[test]
    fn margin_guard_snapshot_deserializes_missing_account_capacity_constraint_with_default() {
        let legacy_snapshot = json!({
            "track_id": "grid-1",
            "instrument": { "venue": "binance", "symbol": "BTCUSDT" },
            "config": {
                "lower_price": 90.0,
                "upper_price": 110.0,
                "long_exposure_units": 8.0,
                "short_exposure_units": 8.0,
                "notional_per_unit": 375.0,
                "shape_family": "linear",
                "out_of_band_policy": "freeze"
            },
            "status": "active",
            "current_exposure": 4.0,
            "target_exposure": 6.0,
            "executor_state": {
                "mode": "passive",
                "inventory_gap": 2.0,
                "gap_started_at": null,
                "last_reprice_at": null,
                "slots": [{
                    "slot": "inventory_core",
                    "state": "empty",
                    "working_order": null
                }],
                "last_execution_reason": null,
                "recovery_anomaly": null,
                "stats": {
                    "started_at": "2026-03-29T09:00:00Z",
                    "max_inventory_gap_abs": 0.0,
                    "max_gap_age_ms": 0
                }
            },
            "replacement_gate_reason": null,
            "risk": {
                "realized_pnl_day": null,
                "realized_pnl_today": 0.0,
                "realized_pnl_cumulative": 0.0,
                "unrealized_pnl": 0.0
            },
            "observed": {
                "reference_price": 96.0,
                "out_of_band_since": null,
                "last_tick_at": null,
                "market_data_stale_since": null
            }
        });

        let restored: TrackRuntimeSnapshot = serde_json::from_value(legacy_snapshot).unwrap();

        assert_eq!(
            restored.risk.account_capacity_constraint,
            AccountCapacityConstraint::default()
        );
    }

    #[test]
    fn snapshot_deserializes_legacy_target_exposure_into_desired_exposure() {
        let legacy_snapshot = json!({
            "track_id": "grid-1",
            "instrument": { "venue": "binance", "symbol": "BTCUSDT" },
            "config": {
                "lower_price": 90.0,
                "upper_price": 110.0,
                "long_exposure_units": 8.0,
                "short_exposure_units": 8.0,
                "notional_per_unit": 375.0,
                "shape_family": "linear",
                "out_of_band_policy": "freeze"
            },
            "status": "active",
            "current_exposure": 4.0,
            "target_exposure": 6.0,
            "executor_state": {
                "mode": "passive",
                "inventory_gap": 0.0,
                "gap_started_at": null,
                "last_reprice_at": null,
                "slots": [{
                    "slot": "inventory_core",
                    "state": "empty",
                    "working_order": null
                }],
                "last_execution_reason": null,
                "recovery_anomaly": null,
                "stats": {
                    "started_at": "2026-03-29T09:00:00Z",
                    "max_inventory_gap_abs": 0.0,
                    "max_gap_age_ms": 0
                }
            },
            "replacement_gate_reason": null,
            "risk": {
                "realized_pnl_day": null,
                "realized_pnl_today": 0.0,
                "realized_pnl_cumulative": 0.0,
                "unrealized_pnl": 0.0
            },
            "observed": {
                "reference_price": 96.0,
                "out_of_band_since": null,
                "last_tick_at": null,
                "market_data_stale_since": null
            }
        });

        let restored: TrackRuntimeSnapshot = serde_json::from_value(legacy_snapshot).unwrap();

        assert_eq!(restored.desired_exposure, Some(Exposure(6.0)));
    }
}
