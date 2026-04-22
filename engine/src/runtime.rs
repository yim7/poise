use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use poise_core::events::ReplacementGateReason;
use poise_core::risk::{
    CapacityBudget, ExposureIntent, LossGuardSnapshot, RiskOutcome, RiskTerminationCause,
    evaluate_risk_outcome,
};
use poise_core::strategy::{BandBoundary, TrackConfig};
use poise_core::types::{ExchangeRules, Exposure};

use crate::execution_gate::ExecutionGateState;
use crate::executor::RecoveryAnomaly;
use crate::executor::binding::LiveOrderBinding;
use crate::executor::boundary::{ProfileRevision, profile_revision_for_config};
use crate::executor::ledger::BoundaryLedgerState;
use crate::ledger::TrackLedgerState;
use crate::persisted_runtime::{PostRestoreConstraints, TrackRestoreRevision, TrackRuntimeSeed};
use crate::ports::ExecutionQuote;
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
pub struct RiskState {
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppliedRiskCap {
    pub intended: Exposure,
    pub capped: Exposure,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrackState {
    WaitingMarketData,
    Running(ControlState),
    Paused { suspended: ControlState },
    Terminated { cause: TerminationCause },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlState {
    Automatic(AutoState),
    Manual(ManualState),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AutoState {
    FollowingBand,
    Frozen {
        target_anchor: Exposure,
    },
    FlattenPending {
        target_anchor: Exposure,
        boundary: BandBoundary,
    },
    Flattening {
        boundary: BandBoundary,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ManualState {
    Flattened,
    TargetOverride { target: Exposure },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TerminationCause {
    ManualCommand,
    Band(BandTerminationCause),
    Risk(RiskTerminationCause),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BandTerminationCause {
    OutOfRange,
}

impl TrackState {
    pub fn status(&self) -> TrackStatus {
        match self {
            Self::WaitingMarketData => TrackStatus::WaitingMarketData,
            Self::Running(ControlState::Automatic(AutoState::FollowingBand)) => TrackStatus::Active,
            Self::Running(ControlState::Automatic(AutoState::Frozen { .. }))
            | Self::Running(ControlState::Automatic(AutoState::FlattenPending { .. })) => {
                TrackStatus::Frozen
            }
            Self::Running(ControlState::Automatic(AutoState::Flattening { .. })) => {
                TrackStatus::Flattening
            }
            Self::Running(ControlState::Manual(ManualState::Flattened)) => {
                TrackStatus::ManualFlattening
            }
            Self::Running(ControlState::Manual(ManualState::TargetOverride { .. })) => {
                TrackStatus::Active
            }
            Self::Paused { .. } => TrackStatus::Paused,
            Self::Terminated { .. } => TrackStatus::Terminated,
        }
    }

    pub fn manual_target_override(&self) -> Option<Exposure> {
        match self {
            Self::Running(ControlState::Manual(ManualState::Flattened)) => Some(Exposure(0.0)),
            Self::Running(ControlState::Manual(ManualState::TargetOverride { target })) => {
                Some(target.clone())
            }
            _ => None,
        }
    }

    pub fn suspended_control_state(&self) -> Option<&ControlState> {
        match self {
            Self::Running(control) => Some(control),
            Self::Paused { suspended } => Some(suspended),
            _ => None,
        }
    }

    pub fn is_terminated(&self) -> bool {
        matches!(self, Self::Terminated { .. })
    }

    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused { .. })
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentTerminalOrder {
    pub client_order_id: String,
    pub order_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutorState {
    pub ledger_state: BoundaryLedgerState,
    pub bindings: Vec<LiveOrderBinding>,
    pub recent_terminal_orders: Vec<RecentTerminalOrder>,
    #[serde(default)]
    pub recovery_anomaly: Option<RecoveryAnomaly>,
}

impl ExecutorState {
    pub fn empty(_started_at: DateTime<Utc>) -> Self {
        Self {
            ledger_state: BoundaryLedgerState {
                profile_revision: ProfileRevision("uninitialized".to_string()),
                ledger_anchor_exposure: Exposure(0.0),
                progress: Vec::new(),
            },
            bindings: Vec::new(),
            recent_terminal_orders: Vec::new(),
            recovery_anomaly: None,
        }
    }

    pub fn reset_for_activation(&self, _started_at: DateTime<Utc>) -> Self {
        let mut reset = self.clone();
        reset.bindings.clear();
        reset.recovery_anomaly = None;
        reset
    }

    pub fn ensure_revision(&self, config: &TrackConfig, current_exposure: Exposure) -> Self {
        let revision = profile_revision_for_config(config);
        if self.ledger_state.profile_revision == revision {
            return self.clone();
        }

        Self {
            ledger_state: BoundaryLedgerState {
                profile_revision: revision,
                ledger_anchor_exposure: current_exposure,
                progress: Vec::new(),
            },
            bindings: Vec::new(),
            recent_terminal_orders: self.recent_terminal_orders.clone(),
            recovery_anomaly: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrackRuntime {
    pub(crate) id: TrackId,
    pub(crate) instrument: Instrument,
    pub(crate) config: TrackConfig,
    pub(crate) budget: CapacityBudget,
    pub(crate) exchange_rules: ExchangeRules,
    pub(crate) track_state: TrackState,
    pub(crate) current_exposure: Exposure,
    // Reconcile owns desired_exposure; exchange sync/restore own observed order and risk fields.
    pub(crate) desired_exposure: Option<Exposure>,
    pub(crate) active_risk_cap: Option<AppliedRiskCap>,
    pub(crate) executor_state: ExecutorState,
    pub(crate) replacement_gate_reason: Option<ReplacementGateReason>,
    pub(crate) ledger_state: TrackLedgerState,
    pub(crate) risk_state: RiskState,
    pub(crate) execution_gate_state: ExecutionGateState,
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
            track_state: TrackState::WaitingMarketData,
            current_exposure: Exposure(0.0),
            desired_exposure: None,
            active_risk_cap: None,
            executor_state: ExecutorState::empty(started_at),
            replacement_gate_reason: None,
            ledger_state: TrackLedgerState::default(),
            risk_state: RiskState::default(),
            execution_gate_state: ExecutionGateState::open(),
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

    pub fn status(&self) -> TrackStatus {
        self.track_state.status()
    }

    pub fn manual_target_override(&self) -> Option<Exposure> {
        self.track_state.manual_target_override()
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
            runtime_state: self.track_state.clone(),
            current_exposure: self.current_exposure.clone(),
            desired_exposure: self.desired_exposure.clone(),
            executor_state: self.executor_state.clone(),
            replacement_gate_reason: self.replacement_gate_reason.clone(),
            execution_gate_state: self.execution_gate_state.clone(),
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

        self.track_state = snapshot.runtime_state.clone();
        self.current_exposure = snapshot.current_exposure.clone();
        self.desired_exposure = snapshot.desired_exposure.clone();
        self.active_risk_cap = None;
        self.executor_state = snapshot.executor_state.clone();
        self.replacement_gate_reason = snapshot.replacement_gate_reason.clone();
        self.ledger_state = snapshot.ledger_state.clone();
        self.risk_state = snapshot.risk.clone();
        self.execution_gate_state = snapshot.execution_gate_state.clone();
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
        if self.track_state.is_paused() {
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
            let decision = evaluate_risk_outcome(
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
                RiskOutcome::Allow { target } | RiskOutcome::Cap { target } => target,
                RiskOutcome::Terminate(_) => Exposure(0.0),
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

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::executor::binding::{BindingProposalKey, BindingStatus, LiveOrderBinding};
    use crate::executor::boundary::{BoundaryDirection, BoundaryId, BoundaryOperation};
    use crate::executor::policy::PolicyKind;
    use crate::ports::OrderRequest;
    use crate::price_gate::SubmitPurpose;
    use crate::track::Venue;

    #[test]
    fn snapshot_round_trips_boundary_ledger_state_and_bindings() {
        let mut state = ExecutorState::empty(Utc::now());
        let boundary_id = BoundaryId {
            profile_revision: state.ledger_state.profile_revision.clone(),
            lower_exposure_bp: 0,
            upper_exposure_bp: 10_000,
        };
        let operation = BoundaryOperation {
            boundary_id,
            direction: BoundaryDirection::Up,
        };
        state.bindings.push(LiveOrderBinding {
            binding_id: "binding-1".to_string(),
            proposal_key: BindingProposalKey {
                policy: PolicyKind::CatchUp,
                operations: vec![operation],
            },
            allocations: Vec::new(),
            request: OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: poise_core::types::Side::Buy,
                price: 100.0,
                quantity: 1.0,
                client_order_id: "client-1".to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(1.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            order_id: None,
            status: BindingStatus::Working,
        });

        let json = serde_json::to_string(&state).unwrap();
        let restored: ExecutorState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored, state);
        let value = serde_json::to_value(restored).unwrap();
        assert!(value.get("active_round").is_none());
        assert!(value.get("slots").is_none());
        assert!(value.get("ledger_state").is_some());
        assert!(value.get("bindings").is_some());
    }
}

fn validate_snapshot_invariants(snapshot: &TrackRuntimeSnapshot) -> Result<()> {
    if matches!(
        snapshot.runtime_state,
        TrackState::Running(ControlState::Manual(ManualState::Flattened))
    ) && snapshot.runtime_state.manual_target_override() != Some(Exposure(0.0))
    {
        anyhow::bail!("manual_flattening requires manual_target_override = 0");
    }

    Ok(())
}
