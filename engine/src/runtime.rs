use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use grid_core::events::ReplacementGateReason;
use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;
use grid_core::types::{ExchangeRules, Exposure, Side};

use crate::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot, RecoveryAnomaly};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitRecoveryKind {
    Submitting,
    ReceiptBacked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitRecoveryAnchor {
    pub client_order_id: String,
    pub kind: SubmitRecoveryKind,
}

impl SubmitRecoveryAnchor {
    pub fn from_executor_state(executor_state: &ExecutorState) -> Option<Self> {
        executor_state
            .slots
            .iter()
            .filter_map(|slot| slot.working_order.as_ref())
            .find_map(|order| {
                if order.order_id.is_none() && order.status == OrderStatus::Submitting {
                    return Some(Self {
                        client_order_id: order.client_order_id.clone(),
                        kind: SubmitRecoveryKind::Submitting,
                    });
                }

                (order.order_id.is_some() && order.status.keeps_working_order()).then(|| Self {
                    client_order_id: order.client_order_id.clone(),
                    kind: SubmitRecoveryKind::ReceiptBacked,
                })
            })
    }
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
    pub executor_state: Option<ExecutorState>,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub risk_state: RiskState,
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

impl GridRuntime {
    pub fn new(
        id: GridId,
        instrument: Instrument,
        config: GridConfig,
        budget: CapacityBudget,
        exchange_rules: ExchangeRules,
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
            executor_state: None,
            replacement_gate_reason: None,
            risk_state: RiskState::default(),
            reference_price: None,
            out_of_band_since: None,
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
            executor_state: self.executor_state.clone(),
            replacement_gate_reason: self.replacement_gate_reason.clone(),
            risk: self.risk_state.clone(),
            observed: ObservedState {
                reference_price: self.reference_price,
                out_of_band_since: self.out_of_band_since,
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
        self.executor_state = snapshot.executor_state.clone();
        self.replacement_gate_reason = snapshot.replacement_gate_reason.clone();
        self.risk_state = snapshot.risk.clone();
        self.reference_price = snapshot.observed.reference_price;
        self.out_of_band_since = snapshot.observed.out_of_band_since;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use grid_core::events::ReplacementGateReason;
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{ExchangeRules, Exposure, Side};

    use crate::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot};
    use crate::grid::{GridId, Instrument, Venue};
    use crate::ports::OrderStatus;

    use super::{
        ExecutionSlot, ExecutionStats, ExecutorState, GridRuntime, GridStatus, RiskState,
        SlotState, SubmitRecoveryAnchor, SubmitRecoveryKind, WorkingOrder,
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
            },
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
    fn submit_recovery_anchor_detects_submitting_and_receipt_backed_slots() {
        let mut state = test_executor_state();
        state.slots[0].state = SlotState::SubmitPending;
        state.slots[0].working_order.as_mut().unwrap().order_id = None;
        state.slots[0].working_order.as_mut().unwrap().status = OrderStatus::Submitting;

        assert_eq!(
            SubmitRecoveryAnchor::from_executor_state(&state),
            Some(SubmitRecoveryAnchor {
                client_order_id: "client-1".into(),
                kind: SubmitRecoveryKind::Submitting,
            })
        );

        let mut receipt_backed = test_executor_state();
        receipt_backed.slots[0].state = SlotState::Working;
        receipt_backed.slots[0]
            .working_order
            .as_mut()
            .unwrap()
            .status = OrderStatus::New;

        assert_eq!(
            SubmitRecoveryAnchor::from_executor_state(&receipt_backed),
            Some(SubmitRecoveryAnchor {
                client_order_id: "client-1".into(),
                kind: SubmitRecoveryKind::ReceiptBacked,
            })
        );

        let mut terminal = test_executor_state();
        terminal.slots[0].working_order.as_mut().unwrap().status = OrderStatus::Filled;

        assert_eq!(SubmitRecoveryAnchor::from_executor_state(&terminal), None);
    }

    #[test]
    fn snapshot_round_trips_executor_state() {
        let mut runtime = test_runtime();
        runtime.status = GridStatus::Active;
        runtime.current_exposure = Exposure(4.0);
        runtime.target_exposure = Some(Exposure(6.0));
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
        runtime.executor_state = Some(test_executor_state());

        let snapshot = runtime.snapshot();
        assert!(snapshot.executor_state.is_some());
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
        restored.restore_from_snapshot(&snapshot).unwrap();

        assert_eq!(restored.status, GridStatus::Active);
        assert_eq!(restored.current_exposure, Exposure(4.0));
        assert_eq!(restored.target_exposure, Some(Exposure(6.0)));
        assert_eq!(restored.executor_state, runtime.executor_state);
        assert_eq!(
            restored.executor_state.as_ref().unwrap().stats.started_at,
            runtime.executor_state.as_ref().unwrap().stats.started_at
        );
    }
}
