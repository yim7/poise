use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use poise_core::events::ReplacementGateReason;
use poise_core::risk::{
    CapacityBudget, ExposureIntent, LossGuardSnapshot, RiskDecision, evaluate_risk,
};
use poise_core::strategy::TrackConfig;
use poise_core::types::{ExchangeRules, Exposure, Side};

use crate::executor::{
    ExecutionMode, ExecutionReason, INVENTORY_CORE_SLOT, OrderRole, OrderSlot, RecoveryAnomaly,
};
use crate::ledger::TrackLedgerState;
use crate::persisted_runtime::{PostRestoreConstraints, TrackRestoreRevision, TrackRuntimeSeed};
use crate::ports::{ExecutionQuote, OrderStatus};
use crate::price_gate::{
    PriceExecutionBlockReason, PriceExecutionGate, evaluate_price_execution_gate,
};
use crate::reconciler;
use crate::snapshot::{ObservedState, TrackRuntimeSnapshot};
use crate::track::{Instrument, TrackId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackStatus {
    WaitingMarketData,
    Active,
    Frozen,
    Holding,
    Flattening,
    ManualFlattening,
    Terminated,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StrategyPriceStatus {
    Live,
    #[default]
    Stale,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AccountCapacityConstraint {
    pub increase_blocked: bool,
    pub blocked_reason: Option<String>,
    pub max_increase_notional: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskState {
    pub unrealized_pnl: f64,
    pub account_capacity_constraint: AccountCapacityConstraint,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppliedRiskCap {
    pub intended: Exposure,
    pub capped: Exposure,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveQuoteState {
    pub strategy_price: Option<f64>,
    pub mark_price: Option<f64>,
    pub execution_quote: Option<ExecutionQuote>,
    pub last_tick_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StrategyTargetView {
    pub desired_exposure: Option<Exposure>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuoteHealthView {
    pub strategy_price_status: StrategyPriceStatus,
    pub price_execution_gate: PriceExecutionGate,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackLiveView {
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub desired_exposure: Option<f64>,
    pub price_execution_block_reason: Option<PriceExecutionBlockReason>,
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
    pub desired_exposure: Exposure,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub(crate) active_risk_cap: Option<AppliedRiskCap>,
    pub(crate) manual_target_override: Option<Exposure>,
    pub(crate) executor_state: ExecutorState,
    pub(crate) replacement_gate_reason: Option<ReplacementGateReason>,
    pub(crate) ledger_state: TrackLedgerState,
    pub(crate) risk_state: RiskState,
    pub(crate) strategy_price: Option<f64>,
    pub(crate) strategy_price_status: StrategyPriceStatus,
    pub(crate) mark_price: Option<f64>,
    pub(crate) best_bid: Option<f64>,
    pub(crate) best_ask: Option<f64>,
    pub(crate) price_execution_gate: PriceExecutionGate,
    pub(crate) out_of_band_since: Option<DateTime<Utc>>,
    pub(crate) last_tick_at: Option<DateTime<Utc>>,
    pub(crate) market_data_stale_since: Option<DateTime<Utc>>,
    pub(crate) tick_timeout_secs: u64,
}

impl TrackRuntime {
    fn bootstrap_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.0,
            quantity_step: 0.0,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

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
            active_risk_cap: None,
            manual_target_override: None,
            executor_state: ExecutorState::empty(started_at),
            replacement_gate_reason: None,
            ledger_state: TrackLedgerState::default(),
            risk_state: RiskState::default(),
            strategy_price: None,
            strategy_price_status: StrategyPriceStatus::Stale,
            mark_price: None,
            best_bid: None,
            best_ask: None,
            price_execution_gate: PriceExecutionGate::NoSubmit {
                reason: PriceExecutionBlockReason::MissingExecutionQuote,
            },
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

    pub fn initial_from_seed(
        seed: TrackRuntimeSeed,
        exchange_rules: ExchangeRules,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self::with_tick_timeout_secs(
            seed.track_id,
            seed.instrument,
            seed.track_config,
            seed.budget,
            exchange_rules,
            started_at,
            seed.tick_timeout_secs,
        )
    }

    pub fn prepare_bootstrap_snapshot(
        seed: TrackRuntimeSeed,
        persisted_snapshot: Option<&TrackRuntimeSnapshot>,
        constraints: PostRestoreConstraints,
        started_at: DateTime<Utc>,
    ) -> Result<TrackRuntimeSnapshot> {
        let mut runtime =
            Self::initial_from_seed(seed, Self::bootstrap_exchange_rules(), started_at);
        if let Some(snapshot) = persisted_snapshot {
            runtime.restore_from_snapshot(snapshot)?;
        }
        runtime.apply_post_restore_constraints(constraints);
        Ok(runtime.snapshot())
    }

    pub fn snapshot(&self) -> TrackRuntimeSnapshot {
        TrackRuntimeSnapshot {
            track_id: self.id.clone(),
            restore_revision: TrackRestoreRevision::for_track(&self.instrument, &self.config),
            status: self.status.clone(),
            current_exposure: self.current_exposure.clone(),
            desired_exposure: self.desired_exposure.clone(),
            manual_target_override: self.manual_target_override.clone(),
            executor_state: self.executor_state.clone(),
            replacement_gate_reason: self.replacement_gate_reason.clone(),
            price_execution_block_reason: None,
            ledger_state: self.ledger_state.clone(),
            risk: self.risk_state.clone(),
            observed: ObservedState {
                strategy_price: None,
                strategy_price_status: StrategyPriceStatus::Stale,
                mark_price: None,
                best_bid: None,
                best_ask: None,
                out_of_band_since: self.out_of_band_since,
                last_tick_at: None,
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
        let expected_revision = TrackRestoreRevision::for_track(&self.instrument, &self.config);
        if expected_revision != snapshot.restore_revision {
            anyhow::bail!(
                "snapshot restore revision mismatch for `{}`",
                self.id.as_str()
            );
        }
        validate_snapshot_invariants(snapshot)?;

        self.status = snapshot.status.clone();
        self.current_exposure = snapshot.current_exposure.clone();
        self.desired_exposure = snapshot.desired_exposure.clone();
        self.active_risk_cap = None;
        self.manual_target_override = snapshot.manual_target_override.clone();
        self.executor_state = snapshot.executor_state.clone();
        self.replacement_gate_reason = snapshot.replacement_gate_reason.clone();
        self.ledger_state = snapshot.ledger_state.clone();
        self.risk_state = snapshot.risk.clone();
        self.strategy_price = None;
        self.strategy_price_status = StrategyPriceStatus::Stale;
        self.mark_price = None;
        self.best_bid = None;
        self.best_ask = None;
        self.price_execution_gate = PriceExecutionGate::NoSubmit {
            reason: PriceExecutionBlockReason::MissingExecutionQuote,
        };
        self.out_of_band_since = snapshot.observed.out_of_band_since;
        self.last_tick_at = None;
        self.market_data_stale_since = snapshot.observed.market_data_stale_since;
        let mut expected_snapshot = snapshot.clone();
        expected_snapshot.price_execution_block_reason = None;
        expected_snapshot.observed.strategy_price = None;
        expected_snapshot.observed.strategy_price_status = StrategyPriceStatus::Stale;
        expected_snapshot.observed.mark_price = None;
        expected_snapshot.observed.best_bid = None;
        expected_snapshot.observed.best_ask = None;
        expected_snapshot.observed.last_tick_at = None;
        debug_assert_eq!(
            self.snapshot(),
            expected_snapshot,
            "restore_from_snapshot left persisted fields unsynced"
        );

        Ok(())
    }

    pub fn live_quote_state(&self) -> LiveQuoteState {
        LiveQuoteState {
            strategy_price: self.strategy_price,
            mark_price: self.mark_price,
            execution_quote: match (self.best_bid, self.best_ask) {
                (Some(best_bid), Some(best_ask)) => Some(ExecutionQuote { best_bid, best_ask }),
                _ => None,
            },
            last_tick_at: self.last_tick_at,
        }
    }

    pub fn quote_health_view(&self) -> QuoteHealthView {
        QuoteHealthView {
            strategy_price_status: if self.strategy_price.is_some() {
                self.strategy_price_status
            } else {
                StrategyPriceStatus::Stale
            },
            price_execution_gate: evaluate_price_execution_gate(
                PriceExecutionGate::Open,
                self.mark_price,
                match (self.best_bid, self.best_ask) {
                    (Some(best_bid), Some(best_ask)) => Some(ExecutionQuote { best_bid, best_ask }),
                    _ => None,
                },
            ),
        }
    }

    pub fn strategy_target_view(&self) -> StrategyTargetView {
        StrategyTargetView {
            desired_exposure: self.live_desired_exposure(),
        }
    }

    pub fn live_view(&self) -> TrackLiveView {
        let quote_health = self.quote_health_view();
        let live_desired_exposure = self.live_desired_exposure();
        TrackLiveView {
            strategy_price: self.strategy_price,
            strategy_price_status: quote_health.strategy_price_status,
            mark_price: self.mark_price,
            best_bid: self.best_bid,
            best_ask: self.best_ask,
            desired_exposure: live_desired_exposure.map(|value| value.0),
            price_execution_block_reason: match quote_health.price_execution_gate {
                PriceExecutionGate::Open => None,
                PriceExecutionGate::ManualRiskReductionOnly { reason }
                | PriceExecutionGate::NoSubmit { reason } => Some(reason),
            },
        }
    }

    fn live_desired_exposure(&self) -> Option<Exposure> {
        if matches!(self.status, TrackStatus::Paused) {
            return None;
        }

        let strategy_price = matches!(self.strategy_price_status, StrategyPriceStatus::Live)
            .then_some(self.strategy_price)
            .flatten()?;

        Some(reconciler::reconcile_target(self, strategy_price).desired_exposure)
    }

    pub fn apply_post_restore_constraints(&mut self, constraints: PostRestoreConstraints) {
        self.budget = constraints.budget;
        self.tick_timeout_secs = constraints.tick_timeout_secs;

        if let Some(target) = self.desired_exposure.clone() {
            let decision = evaluate_risk(
                &ExposureIntent {
                    current: self.current_exposure.clone(),
                    target,
                    unit_notional: self.config.notional_per_unit,
                    loss_guard: LossGuardSnapshot {
                        net_realized_pnl_today: self.ledger_state.net_realized_pnl_today(),
                        net_realized_pnl_cumulative: self
                            .ledger_state
                            .net_realized_pnl_cumulative(),
                        unrealized_pnl: self.risk_state.unrealized_pnl,
                    },
                },
                &self.budget,
            );
            self.desired_exposure = Some(match decision {
                RiskDecision::Allow(exposure) | RiskDecision::Cap(exposure) => exposure,
                RiskDecision::Deny { .. } => Exposure(0.0),
            });
            return;
        }

        let total_loss_amount = (-(self.ledger_state.net_realized_pnl_cumulative()
            + self.risk_state.unrealized_pnl))
            .max(0.0);
        if total_loss_amount >= self.budget.total_loss_limit {
            self.desired_exposure = Some(Exposure(0.0));
        }
    }
}

fn validate_snapshot_invariants(snapshot: &TrackRuntimeSnapshot) -> Result<()> {
    if matches!(snapshot.status, TrackStatus::ManualFlattening)
        && snapshot.manual_target_override != Some(Exposure(0.0))
    {
        anyhow::bail!("manual_flattening requires manual_target_override = 0");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use chrono::{DateTime, TimeZone, Utc};
    use poise_core::events::ReplacementGateReason;
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{BandProtectionPolicy, BandRecoverPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};

    use crate::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot};
    use crate::ledger::TrackLedgerState;
    use crate::persisted_runtime::{
        PersistedRuntimeCodec, PostRestoreConstraints, TrackRestoreRevision, TrackRuntimeSeed,
    };
    use crate::ports::OrderStatus;
    use crate::snapshot::TrackRuntimeSnapshot;
    use crate::track::{Instrument, TrackId, Venue};

    use super::{
        AccountCapacityConstraint, ExecutionRound, ExecutionSlot, ExecutionStats,
        ExecutorDiagnostics, ExecutorState, QuoteHealthView, RiskState, SlotState,
        StrategyPriceStatus, TrackRuntime, TrackStatus, WorkingOrder,
    };
    use crate::price_gate::{PriceExecutionBlockReason, PriceExecutionGate};

    pub(crate) fn test_runtime() -> TrackRuntime {
        TrackRuntime::new(
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
                out_of_band_policy: BandProtectionPolicy::Freeze {
                    recover: BandRecoverPolicy::BackInBand,
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
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        )
    }

    fn test_runtime_seed() -> TrackRuntimeSeed {
        TrackRuntimeSeed {
            track_id: TrackId::new("track-1"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            track_config: TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: BandProtectionPolicy::Freeze {
                    recover: BandRecoverPolicy::BackInBand,
                },
            },
            budget: CapacityBudget {
                max_notional: 6_000.0,
                daily_loss_limit: 500.0,
                total_loss_limit: 1_000.0,
            },
            tick_timeout_secs: 45,
        }
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
                desired_exposure: Exposure(6.0),
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
    fn initial_from_seed_uses_engine_defaults_for_fresh_runtime() {
        let started_at = Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap();
        let runtime = TrackRuntime::initial_from_seed(
            test_runtime_seed(),
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.01,
                min_qty: 0.01,
                min_notional: 5.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
            started_at,
        );

        assert_eq!(runtime.id().as_str(), "track-1");
        assert_eq!(runtime.instrument().symbol, "BTCUSDT");
        assert_eq!(runtime.status(), &TrackStatus::WaitingMarketData);
        assert_eq!(runtime.budget().max_notional, 6_000.0);
        assert_eq!(runtime.tick_timeout_secs, 45);
        assert_eq!(runtime.snapshot().current_exposure, Exposure(0.0));
    }

    #[test]
    fn apply_post_restore_constraints_clears_desired_exposure_when_total_loss_limit_is_breached() {
        let mut runtime = test_runtime();
        runtime.status = TrackStatus::Active;
        runtime.current_exposure = Exposure(4.0);
        runtime.desired_exposure = Some(Exposure(6.0));
        runtime.ledger_state.gross_realized_pnl_cumulative = -1_050.0;
        runtime.risk_state.unrealized_pnl = 0.0;

        runtime.apply_post_restore_constraints(PostRestoreConstraints {
            budget: CapacityBudget {
                max_notional: 6_000.0,
                daily_loss_limit: 500.0,
                total_loss_limit: 1_000.0,
            },
            tick_timeout_secs: 60,
        });

        assert_eq!(runtime.desired_exposure, Some(Exposure(0.0)));
        assert_eq!(runtime.tick_timeout_secs, 60);
    }

    #[test]
    fn prepare_bootstrap_snapshot_restores_and_applies_constraints_without_exchange_rules_input() {
        let mut runtime = test_runtime();
        runtime.status = TrackStatus::Active;
        runtime.current_exposure = Exposure(4.0);
        runtime.desired_exposure = Some(Exposure(6.0));
        runtime.ledger_state.gross_realized_pnl_cumulative = -1_050.0;
        let snapshot = runtime.snapshot();

        let prepared = TrackRuntime::prepare_bootstrap_snapshot(
            test_runtime_seed(),
            Some(&snapshot),
            PostRestoreConstraints {
                budget: CapacityBudget {
                    max_notional: 6_000.0,
                    daily_loss_limit: 500.0,
                    total_loss_limit: 1_000.0,
                },
                tick_timeout_secs: 60,
            },
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 30, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(prepared.restore_revision, snapshot.restore_revision);
        assert_eq!(prepared.current_exposure, Exposure(4.0));
        assert_eq!(prepared.desired_exposure, Some(Exposure(0.0)));
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
            unrealized_pnl: -0.5,
            account_capacity_constraint: AccountCapacityConstraint {
                increase_blocked: true,
                blocked_reason: Some("insufficient_margin".into()),
                max_increase_notional: Some(1_500.0),
            },
            ..RiskState::default()
        };
        runtime.ledger_state = TrackLedgerState {
            gross_realized_pnl_today: 1.0,
            gross_realized_pnl_cumulative: 2.0,
            ..TrackLedgerState::default()
        };
        runtime.strategy_price = Some(96.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
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
        assert_eq!(restored.last_tick_at, None);
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
    fn risk_state_snapshot_no_longer_persists_realized_pnl_fields() {
        let snapshot = test_runtime().snapshot();
        let json = serde_json::to_value(snapshot).unwrap();

        assert!(json["risk"].get("realized_pnl_today").is_none());
        assert!(json["risk"].get("realized_pnl_cumulative").is_none());
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
        runtime.active_risk_cap = Some(super::AppliedRiskCap {
            intended: Exposure(8.0),
            capped: Exposure(4.0),
        });
        runtime.strategy_price = Some(96.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.executor_state = test_executor_state();

        let snapshot = runtime.snapshot();
        let mut fresh = test_runtime();
        fresh.restore_from_snapshot(&snapshot).unwrap();

        assert_eq!(fresh.active_risk_cap, None);
        assert_eq!(fresh.snapshot(), snapshot);
    }

    #[test]
    fn restore_from_snapshot_restores_durable_desired_exposure_but_not_live_quote_or_live_target() {
        let mut runtime = test_runtime();
        runtime.status = TrackStatus::Active;
        runtime.current_exposure = Exposure(4.0);
        runtime.desired_exposure = Some(Exposure(6.0));
        runtime.strategy_price = Some(96.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.mark_price = Some(96.1);
        runtime.best_bid = Some(95.9);
        runtime.best_ask = Some(96.1);
        runtime.last_tick_at = Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 3, 0).unwrap());
        runtime.executor_state = test_executor_state();

        let snapshot = runtime.snapshot();
        let mut fresh = test_runtime();
        fresh.restore_from_snapshot(&snapshot).unwrap();

        assert_eq!(fresh.desired_exposure, Some(Exposure(6.0)));
        assert_eq!(fresh.strategy_price, None);
        assert_eq!(fresh.mark_price, None);
        assert_eq!(fresh.best_bid, None);
        assert_eq!(fresh.best_ask, None);
        assert_eq!(fresh.last_tick_at, None);
        assert_eq!(fresh.strategy_target_view().desired_exposure, None);
        assert_eq!(fresh.live_view().desired_exposure, None);
        assert_eq!(
            fresh.price_execution_gate,
            PriceExecutionGate::NoSubmit {
                reason: PriceExecutionBlockReason::MissingExecutionQuote,
            }
        );
    }

    #[test]
    fn quote_health_view_returns_missing_quote_baseline_without_tick() {
        let runtime = test_runtime();

        assert_eq!(
            runtime.quote_health_view(),
            QuoteHealthView {
                strategy_price_status: StrategyPriceStatus::Stale,
                price_execution_gate: PriceExecutionGate::NoSubmit {
                    reason: PriceExecutionBlockReason::MissingExecutionQuote,
                },
            }
        );
    }

    #[test]
    fn restore_from_snapshot_rejects_manual_flattening_without_zero_override() {
        let mut runtime = test_runtime();
        runtime.status = TrackStatus::ManualFlattening;
        runtime.desired_exposure = Some(Exposure(0.0));
        runtime.manual_target_override = None;

        let snapshot = runtime.snapshot();
        let mut fresh = test_runtime();
        let error = fresh.restore_from_snapshot(&snapshot).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("manual_flattening requires manual_target_override = 0"),
            "unexpected error: {error:#}"
        );
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
    fn margin_guard_snapshot_rejects_legacy_snapshot_without_restore_revision() {
        let legacy_snapshot = json!({
            "track_id": "track-1",
            "status": "active",
            "current_exposure": 4.0,
            "desired_exposure": 6.0,
            "executor_state": serde_json::to_value(test_executor_state()).unwrap(),
            "replacement_gate_reason": null,
            "risk": {
                "unrealized_pnl": 0.0,
                "account_capacity_constraint": {
                    "increase_blocked": false,
                    "blocked_reason": null,
                    "max_increase_notional": null
                }
            },
            "observed": {
                "strategy_price": 96.0,
                "strategy_price_status": "live",
                "out_of_band_since": null,
                "last_tick_at": null,
                "market_data_stale_since": null
            }
        });

        let error = PersistedRuntimeCodec::decode(legacy_snapshot)
            .expect_err("legacy snapshot without restore_revision should fail");
        let rendered = format!("{error:#}");

        assert!(
            rendered.contains("restore_revision"),
            "unexpected error: {rendered}"
        );
    }

    #[test]
    fn restore_from_legacy_snapshot_with_realized_fields_is_rejected() {
        let legacy_snapshot = json!({
            "track_id": "track-1",
            "status": "active",
            "current_exposure": 4.0,
            "desired_exposure": 6.0,
            "executor_state": serde_json::to_value(test_executor_state()).unwrap(),
            "replacement_gate_reason": null,
            "ledger_state": {
                "realized_pnl_day": "2026-04-08",
                "gross_realized_pnl_today": 120.0,
                "gross_realized_pnl_cumulative": 300.0,
                "trading_fee_cumulative": 5.0,
                "funding_fee_cumulative": -2.0,
                "unresolved_gaps": []
            },
            "risk": {
                "realized_pnl_day": "2026-04-08",
                "realized_pnl_today": 120.0,
                "realized_pnl_cumulative": 300.0,
                "unrealized_pnl": -30.0,
                "account_capacity_constraint": {
                    "increase_blocked": false,
                    "blocked_reason": null,
                    "max_increase_notional": null
                }
            },
            "observed": {}
        });

        let error = PersistedRuntimeCodec::decode(legacy_snapshot)
            .expect_err("legacy snapshot should fail");
        let rendered = format!("{error:#}");

        assert!(
            rendered.contains("restore_revision"),
            "unexpected error: {rendered}"
        );
    }

    #[test]
    fn snapshot_deserializes_desired_exposure() {
        let snapshot = json!({
            "track_id": "track-1",
            "restore_revision": TrackRestoreRevision::for_track(
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                &test_runtime_seed().track_config
            ).as_str(),
            "status": "active",
            "current_exposure": 4.0,
            "desired_exposure": 6.0,
            "executor_state": serde_json::to_value(test_executor_state()).unwrap(),
            "replacement_gate_reason": null,
            "ledger_state": TrackLedgerState::default(),
            "risk": {
                "unrealized_pnl": 0.0,
                "account_capacity_constraint": {
                    "increase_blocked": false,
                    "blocked_reason": null,
                    "max_increase_notional": null
                }
            },
            "observed": {
                "strategy_price": 96.0,
                "strategy_price_status": "live",
                "out_of_band_since": null,
                "last_tick_at": null,
                "market_data_stale_since": null
            }
        });

        let restored: TrackRuntimeSnapshot = serde_json::from_value(snapshot).unwrap();

        assert_eq!(restored.desired_exposure, Some(Exposure(6.0)));
    }

    fn future_ledger_snapshot_json() -> serde_json::Value {
        json!({
            "track_id": "track-1",
            "restore_revision": TrackRestoreRevision::for_track(
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                &test_runtime_seed().track_config
            ).as_str(),
            "status": "active",
            "current_exposure": 4.0,
            "desired_exposure": 6.0,
            "executor_state": serde_json::to_value(test_executor_state()).unwrap(),
            "replacement_gate_reason": null,
            "risk": {
                "unrealized_pnl": -0.5,
                "account_capacity_constraint": {
                    "increase_blocked": false,
                    "blocked_reason": null,
                    "max_increase_notional": null
                }
            },
            "ledger_state": {
                "realized_pnl_day": "2026-03-29",
                "gross_realized_pnl_today": 1.0,
                "gross_realized_pnl_cumulative": 2.0,
                "trading_fee_today": 0.3,
                "trading_fee_cumulative": 0.3,
                "funding_fee_today": -0.1,
                "funding_fee_cumulative": -0.1,
                "unresolved_gaps": [
                    {
                        "gap_key": "binance:order_trade_update:btcusdt:12345:commission_asset",
                        "reason": "unsupported_commission_asset",
                        "observed_at": "2026-03-29T08:00:00Z",
                        "source": "binance:order_trade_update"
                    },
                    {
                        "gap_key": "binance:funding_fee:btcusdt:2026-03-29T08:05:00Z:missing_symbol",
                        "reason": "missing_symbol",
                        "observed_at": "2026-03-29T08:05:00Z",
                        "source": "binance:account_update"
                    }
                ]
            },
            "observed": {
                "strategy_price": 96.0,
                "strategy_price_status": "live",
                "out_of_band_since": null,
                "last_tick_at": null,
                "market_data_stale_since": null
            }
        })
    }

    #[test]
    fn unresolved_gaps_accumulate_without_overwriting_previous_records() {
        let restored = PersistedRuntimeCodec::decode(future_ledger_snapshot_json()).unwrap();
        let roundtrip = serde_json::to_value(restored).unwrap();
        let gaps = roundtrip["ledger_state"]["unresolved_gaps"]
            .as_array()
            .expect("ledger snapshot should persist unresolved gaps");

        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0]["reason"], json!("unsupported_commission_asset"));
        assert_eq!(gaps[1]["reason"], json!("missing_symbol"));
    }

    #[test]
    fn ledger_state_owns_daily_realized_window() {
        let restored = PersistedRuntimeCodec::decode(future_ledger_snapshot_json()).unwrap();
        let roundtrip = serde_json::to_value(restored).unwrap();

        assert_eq!(
            roundtrip["ledger_state"]["realized_pnl_day"],
            json!("2026-03-29")
        );
        assert_eq!(
            roundtrip["ledger_state"]["gross_realized_pnl_today"],
            json!(1.0)
        );
        assert_eq!(
            roundtrip["ledger_state"]["gross_realized_pnl_cumulative"],
            json!(2.0)
        );
    }

    #[test]
    fn track_runtime_snapshot_roundtrip_preserves_ledger_state() {
        let restored = PersistedRuntimeCodec::decode(future_ledger_snapshot_json()).unwrap();
        let roundtrip = serde_json::to_value(restored).unwrap();

        assert_eq!(roundtrip["ledger_state"]["trading_fee_today"], json!(0.3));
        assert_eq!(roundtrip["ledger_state"]["funding_fee_today"], json!(-0.1));
    }

    #[test]
    fn ledger_gap_record_has_stable_gap_key() {
        let restored = PersistedRuntimeCodec::decode(future_ledger_snapshot_json()).unwrap();
        let roundtrip = serde_json::to_value(restored).unwrap();
        let first_gap = &roundtrip["ledger_state"]["unresolved_gaps"][0];

        assert_eq!(
            first_gap["gap_key"],
            json!("binance:order_trade_update:btcusdt:12345:commission_asset")
        );
    }
}
