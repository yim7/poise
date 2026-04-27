use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use poise_core::events::ExecutionGateReason;
use poise_core::risk::{LossLimits, RiskTerminationCause};
use poise_core::strategy::{BandBoundary, TrackConfig};
use poise_core::types::{ExchangeRules, Exposure, Side};

use crate::execution_gate::{ExecutionGateDecision, ExecutionGateState};
use crate::executor::RecoveryAnomaly;
use crate::executor::binding::{BindingStatus, LiveOrderBinding};
use crate::executor::boundary::{ProfileRevision, profile_revision_for_config};
use crate::executor::ledger::BoundaryLedgerState;
use crate::executor::policy::PolicyKind;
use crate::ledger::TrackLedgerState;
use crate::mutation_frame::{TrackMutationFrame, TrackMutationFrameRevision};
use crate::ports::ExecutionQuote;
use crate::price_gate::{
    PriceExecutionBlockReason, PriceExecutionGate, evaluate_price_execution_gate,
};
use crate::reconciler;
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

#[derive(Debug, Clone, PartialEq)]
pub struct CurrentMarketData {
    pub strategy_price: f64,
    pub mark_price: Option<f64>,
    pub execution_quote: ExecutionQuote,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FreshSessionExternalInputs {
    pub current_exposure: Exposure,
    pub market_data: Option<CurrentMarketData>,
    pub exchange_rules: ExchangeRules,
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExecutorView {
    pub bindings: Vec<BindingView>,
    pub recovery_anomaly: Option<RecoveryAnomaly>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BindingView {
    pub id: String,
    pub policy: PolicyKind,
    pub is_passive_execution: bool,
    pub status: BindingStatus,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub increases_inventory: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRuntimeView {
    pub status: TrackStatus,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub manual_target_override: Option<Exposure>,
    pub executor: ExecutorView,
    pub ledger_state: TrackLedgerState,
    pub unrealized_pnl: f64,
    pub has_account_margin_guard: bool,
    pub price_execution_block_reason: Option<PriceExecutionBlockReason>,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub last_tick_at: Option<DateTime<Utc>>,
    pub market_data_stale_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentTerminalOrder {
    pub client_order_id: String,
    pub order_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutorState {
    pub ledger_state: BoundaryLedgerState,
    pub bindings: Vec<LiveOrderBinding>,
    pub recent_terminal_orders: Vec<RecentTerminalOrder>,
    pub recovery_anomaly: Option<RecoveryAnomaly>,
}

impl ExecutorState {
    pub fn view(&self) -> ExecutorView {
        ExecutorView {
            bindings: self
                .bindings
                .iter()
                .filter(|binding| binding.is_active())
                .map(|binding| BindingView {
                    id: binding.binding_id.clone(),
                    policy: binding.policy(),
                    is_passive_execution: binding.is_passive_execution(),
                    status: binding.status,
                    side: binding.request.side,
                    price: binding.request.price,
                    quantity: binding.request.quantity,
                    increases_inventory: binding.increases_inventory(),
                })
                .collect(),
            recovery_anomaly: self.recovery_anomaly.clone(),
        }
    }

    pub fn empty(_started_at: DateTime<Utc>) -> Self {
        Self {
            ledger_state: BoundaryLedgerState {
                profile_revision: ProfileRevision("uninitialized".to_string()),
                ledger_anchor_exposure: Exposure(0.0),
                progress: Default::default(),
            },
            bindings: Vec::new(),
            recent_terminal_orders: Vec::new(),
            recovery_anomaly: None,
        }
    }

    pub fn reset_for_activation(
        &self,
        config: &TrackConfig,
        current_exposure: Exposure,
        _started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            ledger_state: BoundaryLedgerState {
                profile_revision: profile_revision_for_config(config),
                ledger_anchor_exposure: current_exposure,
                progress: Default::default(),
            },
            bindings: Vec::new(),
            recent_terminal_orders: Vec::new(),
            recovery_anomaly: None,
        }
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
                progress: Default::default(),
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
    pub(crate) max_notional: f64,
    pub(crate) loss_limits: LossLimits,
    pub(crate) exchange_rules: ExchangeRules,
    pub(crate) track_state: TrackState,
    pub(crate) current_exposure: Exposure,
    // Reconcile owns desired_exposure; exchange sync/restore own observed order and risk fields.
    pub(crate) desired_exposure: Option<Exposure>,
    pub(crate) active_risk_cap: Option<AppliedRiskCap>,
    pub(crate) executor_state: ExecutorState,
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
    pub fn new(
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        max_notional: f64,
        loss_limits: LossLimits,
        exchange_rules: ExchangeRules,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self::with_tick_timeout_secs(
            id,
            instrument,
            config,
            max_notional,
            loss_limits,
            exchange_rules,
            started_at,
            30,
        )
    }

    pub fn with_tick_timeout_secs(
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        max_notional: f64,
        loss_limits: LossLimits,
        exchange_rules: ExchangeRules,
        started_at: DateTime<Utc>,
        tick_timeout_secs: u64,
    ) -> Self {
        Self {
            id,
            instrument,
            config,
            max_notional,
            loss_limits,
            exchange_rules,
            track_state: TrackState::WaitingMarketData,
            current_exposure: Exposure(0.0),
            desired_exposure: None,
            active_risk_cap: None,
            executor_state: ExecutorState::empty(started_at),
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

    pub fn max_notional(&self) -> f64 {
        self.max_notional
    }

    pub fn loss_limits(&self) -> &LossLimits {
        &self.loss_limits
    }

    pub fn exchange_rules(&self) -> &ExchangeRules {
        &self.exchange_rules
    }

    pub fn fresh_start(
        &self,
        track_state: TrackState,
        ledger_state: TrackLedgerState,
        external_inputs: FreshSessionExternalInputs,
        started_at: DateTime<Utc>,
    ) -> Self {
        let FreshSessionExternalInputs {
            current_exposure,
            market_data,
            exchange_rules,
        } = external_inputs;
        let mut fresh = Self::with_tick_timeout_secs(
            self.id.clone(),
            self.instrument.clone(),
            self.config.clone(),
            self.max_notional,
            self.loss_limits.clone(),
            exchange_rules,
            started_at,
            self.tick_timeout_secs,
        );
        fresh.track_state = track_state.clone();
        fresh.current_exposure = current_exposure.clone();
        fresh.desired_exposure = track_state.manual_target_override();
        fresh.executor_state =
            ExecutorState::empty(started_at).ensure_revision(&fresh.config, current_exposure);
        fresh.ledger_state = ledger_state;
        fresh.execution_gate_state = ExecutionGateState::open();
        fresh.price_execution_gate = PriceExecutionGate::NoSubmit {
            reason: PriceExecutionBlockReason::MissingExecutionQuote,
        };

        if let Some(market_data) = market_data {
            fresh.strategy_price = Some(market_data.strategy_price);
            fresh.strategy_price_status = StrategyPriceStatus::Live;
            fresh.mark_price = market_data.mark_price;
            fresh.best_bid = Some(market_data.execution_quote.best_bid);
            fresh.best_ask = Some(market_data.execution_quote.best_ask);
            fresh.last_tick_at = Some(market_data.observed_at);
        }

        fresh.price_execution_gate = fresh.quote_health_view().price_execution_gate;
        fresh
    }

    pub fn mutation_frame(&self) -> TrackMutationFrame {
        TrackMutationFrame {
            track_id: self.id.clone(),
            frame_revision: TrackMutationFrameRevision::for_track(&self.instrument, &self.config),
            runtime_state: self.track_state.clone(),
            current_exposure: self.current_exposure.clone(),
            desired_exposure: self.desired_exposure.clone(),
            executor_state: self.executor_state.clone(),
            execution_gate_state: self.execution_gate_state.clone(),
            ledger_state: self.ledger_state.clone(),
            risk: self.risk_state.clone(),
            out_of_band_since: self.out_of_band_since,
            market_data_stale_since: self.market_data_stale_since,
        }
    }

    pub fn rollback_to_frame(&mut self, frame: &TrackMutationFrame) -> Result<()> {
        if self.id != frame.track_id {
            anyhow::bail!(
                "mutation frame track id mismatch: runtime has `{}`, frame has `{}`",
                self.id.as_str(),
                frame.track_id.as_str()
            );
        }
        let expected_revision =
            TrackMutationFrameRevision::for_track(&self.instrument, &self.config);
        if expected_revision != frame.frame_revision {
            anyhow::bail!(
                "mutation frame revision mismatch for `{}`",
                self.id.as_str()
            );
        }
        validate_frame_invariants(frame)?;

        self.track_state = frame.runtime_state.clone();
        self.current_exposure = frame.current_exposure.clone();
        self.desired_exposure = frame.desired_exposure.clone();
        self.active_risk_cap = None;
        self.executor_state = frame.executor_state.clone();
        self.ledger_state = frame.ledger_state.clone();
        self.risk_state = frame.risk.clone();
        self.execution_gate_state = frame.execution_gate_state.clone();
        self.strategy_price = None;
        self.strategy_price_status = StrategyPriceStatus::Stale;
        self.mark_price = None;
        self.best_bid = None;
        self.best_ask = None;
        self.price_execution_gate = PriceExecutionGate::NoSubmit {
            reason: PriceExecutionBlockReason::MissingExecutionQuote,
        };
        self.out_of_band_since = frame.out_of_band_since;
        self.last_tick_at = None;
        self.market_data_stale_since = frame.market_data_stale_since;
        debug_assert_eq!(
            self.mutation_frame(),
            *frame,
            "rollback_to_frame left frame fields unsynced"
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

    pub fn runtime_view(&self) -> TrackRuntimeView {
        let live = self.live_view();
        TrackRuntimeView {
            status: self.status(),
            current_exposure: self.current_exposure.clone(),
            desired_exposure: live
                .desired_exposure
                .as_ref()
                .map(|value| Exposure(*value))
                .or_else(|| self.desired_exposure.clone()),
            manual_target_override: self.manual_target_override(),
            executor: self.executor_state.view(),
            ledger_state: self.ledger_state.clone(),
            unrealized_pnl: self.risk_state.unrealized_pnl,
            has_account_margin_guard: matches!(
                self.execution_gate_state.last_decision,
                ExecutionGateDecision::NoSubmit {
                    reason: ExecutionGateReason::AccountCapacityInsufficient { .. },
                }
            ),
            price_execution_block_reason: live
                .price_execution_block_reason
                .or(self.execution_gate_state.price_execution_block_reason),
            strategy_price: live.strategy_price,
            strategy_price_status: live.strategy_price_status,
            mark_price: live.mark_price,
            best_bid: live.best_bid,
            best_ask: live.best_ask,
            last_tick_at: self.last_tick_at,
            market_data_stale_since: self.market_data_stale_since,
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
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::executor::binding::{
        BindingPolicyState, BindingProposalKey, BindingStatus, LiveOrderBinding,
    };
    use crate::executor::boundary::{BoundaryDirection, BoundaryId, BoundaryOperation};
    use crate::executor::policy::PolicyKind;
    use crate::ports::OrderRequest;
    use crate::price_gate::SubmitPurpose;
    use crate::track::Venue;

    #[test]
    fn runtime_frame_is_current_process_state_not_persisted_document() {
        let mut runtime = TrackRuntime::new(
            "test".into(),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            test_config(),
            10_000.0,
            test_loss_limits(),
            test_rules(0.1),
            Utc::now(),
        );
        runtime.executor_state = dirty_executor_state();

        let runtime_frame = runtime.mutation_frame();
        assert!(
            !runtime_frame
                .executor_state
                .ledger_state
                .progress
                .is_empty(),
            "runtime clones must preserve current-session progress"
        );
    }

    #[test]
    fn fresh_start_discards_local_execution_state_and_reanchors_boundary_ledger() {
        let mut runtime = test_runtime();
        runtime.current_exposure = Exposure(7.0);
        runtime.desired_exposure = Some(Exposure(4.0));
        runtime.executor_state = dirty_executor_state();
        runtime.strategy_price = Some(101.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.mark_price = Some(100.5);
        runtime.best_bid = Some(100.4);
        runtime.best_ask = Some(100.6);
        runtime.last_tick_at = Some(Utc.with_ymd_and_hms(2026, 4, 23, 8, 0, 0).unwrap());
        runtime.market_data_stale_since =
            Some(Utc.with_ymd_and_hms(2026, 4, 23, 7, 59, 0).unwrap());

        let fresh = runtime.fresh_start(
            TrackState::WaitingMarketData,
            TrackLedgerState {
                ledger_utc_day: Utc
                    .with_ymd_and_hms(2026, 4, 23, 0, 0, 0)
                    .unwrap()
                    .date_naive(),
                gross_realized_pnl_cumulative: 55.0,
                ..TrackLedgerState::default()
            },
            FreshSessionExternalInputs {
                current_exposure: Exposure(2.5),
                market_data: Some(CurrentMarketData {
                    strategy_price: 96.0,
                    mark_price: Some(95.8),
                    execution_quote: ExecutionQuote {
                        best_bid: 95.7,
                        best_ask: 95.9,
                    },
                    observed_at: Utc.with_ymd_and_hms(2026, 4, 23, 8, 1, 0).unwrap(),
                }),
                exchange_rules: test_rules(0.5),
            },
            Utc.with_ymd_and_hms(2026, 4, 23, 8, 2, 0).unwrap(),
        );

        assert_eq!(fresh.current_exposure, Exposure(2.5));
        assert_eq!(fresh.exchange_rules.price_tick, 0.5);
        assert_eq!(
            fresh.executor_state.bindings,
            Vec::<LiveOrderBinding>::new()
        );
        assert_eq!(fresh.executor_state.recent_terminal_orders.len(), 0);
        assert!(fresh.executor_state.recovery_anomaly.is_none());
        assert_eq!(
            fresh.executor_state.ledger_state.profile_revision,
            profile_revision_for_config(&test_config())
        );
        assert_eq!(
            fresh.executor_state.ledger_state.ledger_anchor_exposure,
            Exposure(2.5)
        );
        assert!(fresh.executor_state.ledger_state.progress.is_empty());
        assert_eq!(fresh.desired_exposure, None);
        assert_eq!(fresh.strategy_price, Some(96.0));
        assert_eq!(fresh.strategy_price_status, StrategyPriceStatus::Live);
        assert_eq!(fresh.mark_price, Some(95.8));
        assert_eq!(fresh.best_bid, Some(95.7));
        assert_eq!(fresh.best_ask, Some(95.9));
        assert_eq!(
            fresh.last_tick_at,
            Some(Utc.with_ymd_and_hms(2026, 4, 23, 8, 1, 0).unwrap())
        );
        assert!(fresh.market_data_stale_since.is_none());
        assert_eq!(fresh.ledger_state.gross_realized_pnl_cumulative, 55.0);
    }

    #[test]
    fn fresh_start_without_market_data_keeps_waiting_market_data_and_clears_old_quotes() {
        let mut runtime = test_runtime();
        runtime.track_state =
            TrackState::Running(ControlState::Manual(ManualState::TargetOverride {
                target: Exposure(3.0),
            }));
        runtime.desired_exposure = Some(Exposure(3.0));
        runtime.strategy_price = Some(101.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.mark_price = Some(100.5);
        runtime.best_bid = Some(100.4);
        runtime.best_ask = Some(100.6);

        let fresh = runtime.fresh_start(
            TrackState::WaitingMarketData,
            TrackLedgerState::default(),
            FreshSessionExternalInputs {
                current_exposure: Exposure(1.0),
                market_data: None,
                exchange_rules: test_rules(1.0),
            },
            Utc.with_ymd_and_hms(2026, 4, 23, 8, 3, 0).unwrap(),
        );

        assert_eq!(fresh.status(), TrackStatus::WaitingMarketData);
        assert_eq!(fresh.desired_exposure, None);
        assert_eq!(fresh.strategy_price, None);
        assert_eq!(fresh.mark_price, None);
        assert_eq!(fresh.best_bid, None);
        assert_eq!(fresh.best_ask, None);
        assert_eq!(fresh.strategy_price_status, StrategyPriceStatus::Stale);
        assert_eq!(fresh.exchange_rules.price_tick, 1.0);
    }

    #[test]
    fn reset_for_activation_discards_session_execution_state_and_reanchors_boundary_ledger() {
        let state = dirty_executor_state();

        let reset = state.reset_for_activation(
            &test_config(),
            Exposure(2.5),
            Utc.with_ymd_and_hms(2026, 4, 23, 8, 2, 0).unwrap(),
        );

        assert!(reset.bindings.is_empty());
        assert!(reset.recent_terminal_orders.is_empty());
        assert!(reset.recovery_anomaly.is_none());
        assert!(reset.ledger_state.progress.is_empty());
        assert_eq!(
            reset.ledger_state.profile_revision,
            profile_revision_for_config(&test_config())
        );
        assert_eq!(reset.ledger_state.ledger_anchor_exposure, Exposure(2.5));
    }

    fn test_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: poise_core::strategy::ShapeFamily::Linear,
            out_of_band_policy: poise_core::strategy::BandProtectionPolicy::Freeze,
        }
    }

    fn test_loss_limits() -> LossLimits {
        LossLimits {
            daily_loss_limit: 1_000.0,
            total_loss_limit: 5_000.0,
        }
    }

    fn test_rules(price_tick: f64) -> ExchangeRules {
        ExchangeRules {
            price_tick,
            quantity_step: 0.01,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    fn test_runtime() -> TrackRuntime {
        TrackRuntime::new(
            "test".into(),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            test_config(),
            10_000.0,
            test_loss_limits(),
            test_rules(0.1),
            Utc::now(),
        )
    }

    fn dirty_executor_state() -> ExecutorState {
        let mut state =
            ExecutorState::empty(Utc::now()).ensure_revision(&test_config(), Exposure(7.0));
        let boundary_id = BoundaryId {
            profile_revision: state.ledger_state.profile_revision.clone(),
            lower_exposure_bp: 0,
            upper_exposure_bp: 10_000,
        };
        let operation = BoundaryOperation {
            boundary_id,
            direction: BoundaryDirection::Up,
        };
        state.ledger_state.progress.insert(
            operation.boundary_id.clone(),
            crate::executor::ledger::BoundaryProgress {
                cumulative_up: 1.0,
                cumulative_down: 0.0,
            },
        );
        state.bindings.push(LiveOrderBinding {
            binding_id: "binding-1".to_string(),
            proposal_key: BindingProposalKey {
                policy: PolicyKind::CatchUp,
                operations: vec![operation],
            },
            allocations: Vec::new(),
            absorbed_exposure_qty: 0.0,
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
            policy_state: BindingPolicyState::Stateless,
        });
        state.recovery_anomaly = Some(RecoveryAnomaly::UnknownLiveOrder);
        state.recent_terminal_orders.push(RecentTerminalOrder {
            client_order_id: "old-client".into(),
            order_id: "old-order".into(),
        });
        state
    }
}

fn validate_frame_invariants(frame: &TrackMutationFrame) -> Result<()> {
    if matches!(
        frame.runtime_state,
        TrackState::Running(ControlState::Manual(ManualState::Flattened))
    ) && frame.runtime_state.manual_target_override() != Some(Exposure(0.0))
    {
        anyhow::bail!("manual_flattening requires manual_target_override = 0");
    }

    Ok(())
}
