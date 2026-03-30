use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use grid_core::events::ReplacementGateReason;
use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;
use grid_core::types::{ExchangeRules, Exposure, Side};

use crate::executor::{
    ExecutionMode, ExecutionReason, INVENTORY_CORE_SLOT, OrderRole, OrderSlot, RecoveryAnomaly,
};
use crate::grid::{GridId, Instrument};
use crate::ports::OrderStatus;
use crate::snapshot::{GridRuntimeSnapshot, ObservedState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub realized_pnl_today: f64,
    pub realized_pnl_cumulative: f64,
    pub unrealized_pnl: f64,
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
    pub target_exposure: Exposure,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutorState {
    pub mode: ExecutionMode,
    pub inventory_gap: Exposure,
    pub gap_started_at: Option<DateTime<Utc>>,
    pub last_reprice_at: Option<DateTime<Utc>>,
    pub slots: Vec<ExecutionSlot>,
    pub last_execution_reason: Option<ExecutionReason>,
    #[serde(default)]
    pub recovery_anomaly: Option<RecoveryAnomaly>,
    pub stats: ExecutionStats,
}

impl ExecutorState {
    pub fn empty(started_at: DateTime<Utc>) -> Self {
        Self {
            mode: ExecutionMode::Passive,
            inventory_gap: Exposure(0.0),
            gap_started_at: None,
            last_reprice_at: None,
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new(INVENTORY_CORE_SLOT),
                state: SlotState::Empty,
                working_order: None,
            }],
            last_execution_reason: None,
            recovery_anomaly: None,
            stats: ExecutionStats::new(started_at),
        }
    }

    pub fn reset_for_activation(&self, started_at: DateTime<Utc>) -> Self {
        let mut reset = self.clone();
        reset.gap_started_at = (!reset.inventory_gap.is_zero()).then_some(started_at);
        reset.last_reprice_at = None;
        reset.last_execution_reason = None;
        reset.stats = ExecutionStats::new(started_at);
        reset
    }
}

#[derive(Debug, Clone)]
pub struct GridRuntime {
    pub id: GridId,
    pub instrument: Instrument,
    pub config: GridConfig,
    pub budget: CapacityBudget,
    pub exchange_rules: ExchangeRules,
    pub status: GridStatus,
    pub current_exposure: Exposure,
    // Reconcile owns target_exposure; exchange sync/restore own observed order and risk fields.
    pub target_exposure: Option<Exposure>,
    pub manual_target_override: Option<Exposure>,
    pub executor_state: ExecutorState,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub risk_state: RiskState,
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
    pub last_tick_at: Option<DateTime<Utc>>,
    pub market_data_stale_since: Option<DateTime<Utc>>,
    pub tick_timeout_secs: u64,
}

impl GridRuntime {
    pub fn new(
        id: GridId,
        instrument: Instrument,
        config: GridConfig,
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
        id: GridId,
        instrument: Instrument,
        config: GridConfig,
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
            status: GridStatus::WaitingMarketData,
            current_exposure: Exposure(0.0),
            target_exposure: None,
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

    pub fn snapshot(&self) -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: self.id.clone(),
            instrument: self.instrument.clone(),
            config: self.config.clone(),
            status: self.status.clone(),
            current_exposure: self.current_exposure.clone(),
            target_exposure: self.target_exposure.clone(),
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

    pub fn restore_from_snapshot(&mut self, snapshot: &GridRuntimeSnapshot) -> Result<()> {
        if self.id != snapshot.grid_id {
            anyhow::bail!(
                "snapshot grid id mismatch: runtime has `{}`, snapshot has `{}`",
                self.id.as_str(),
                snapshot.grid_id.as_str()
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
        self.target_exposure = snapshot.target_exposure.clone();
        self.manual_target_override = snapshot.manual_target_override.clone();
        self.executor_state = snapshot.executor_state.clone();
        self.replacement_gate_reason = snapshot.replacement_gate_reason.clone();
        self.risk_state = snapshot.risk.clone();
        self.reference_price = snapshot.observed.reference_price;
        self.out_of_band_since = snapshot.observed.out_of_band_since;
        self.last_tick_at = snapshot.observed.last_tick_at;
        self.market_data_stale_since = snapshot.observed.market_data_stale_since;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use grid_core::events::ReplacementGateReason;
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{ExchangeRules, Exposure, Side};

    use crate::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot};
    use crate::grid::{GridId, Instrument, Venue};
    use crate::ports::OrderStatus;

    use super::{
        ExecutionSlot, ExecutionStats, ExecutorState, GridRuntime, GridStatus, RiskState,
        SlotState, WorkingOrder,
    };

    fn test_runtime() -> GridRuntime {
        GridRuntime::new(
            GridId::new("grid-1"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
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
        let slot = ExecutionSlot {
            slot: OrderSlot::new("passive_buy_1"),
            state: SlotState::Working,
            working_order: Some(WorkingOrder {
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: Exposure(6.0),
                status: OrderStatus::New,
                role: OrderRole::IncreaseInventory,
            }),
        };

        ExecutorState {
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
            slots: vec![slot],
            last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
            recovery_anomaly: None,
            stats: ExecutionStats {
                started_at: DateTime::parse_from_rfc3339("2026-03-29T07:55:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                max_inventory_gap_abs: Exposure(3.0),
                max_gap_age_ms: 42_000,
            },
        }
    }

    #[test]
    fn snapshot_round_trips_executor_state() {
        let mut runtime = test_runtime();
        runtime.status = GridStatus::Active;
        runtime.current_exposure = Exposure(4.0);
        runtime.target_exposure = Some(Exposure(6.0));
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

        assert_eq!(restored.status, GridStatus::Active);
        assert_eq!(restored.current_exposure, Exposure(4.0));
        assert_eq!(restored.target_exposure, Some(Exposure(6.0)));
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
    }
}
