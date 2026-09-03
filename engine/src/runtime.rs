use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use poise_core::events::ExecutionGateReason;
use poise_core::instrument::{Amount, InstrumentSpec, PositionBasis, PositionQty};
use poise_core::risk::{LossLimits, RiskTerminationCause};
use poise_core::strategy::{BandBoundary, TrackConfig};
use poise_core::track::{Instrument, TrackDefinition, TrackId};
use poise_core::types::{Exposure, Side};

use crate::exchange::ExecutionQuote;
use crate::execution_gate::{ExecutionGateDecision, ExecutionGateState};
use crate::execution_plan::DecisionReason;
use crate::executor::RecoveryAnomaly;
use crate::executor::binding::{BindingStatus, LiveOrderBinding};
use crate::executor::boundary::trigger_price_for_boundary;
use crate::executor::boundary::{ProfileRevision, profile_revision_for_config};
use crate::executor::ledger::BoundaryLedgerState;
use crate::executor::policy::PolicyKind;
use crate::ledger::{TrackPnlStats, TrackPnlView};
use crate::price_gate::{
    PriceExecutionBlockReason, PriceExecutionGate, evaluate_price_execution_gate,
};
use crate::reconciler;
use crate::risk_exposure_gate::{
    self, AppliedRiskAcquisitionRelease, FreshAcquisitionInitialization, RiskAcquisitionRelease,
    RiskExposureGateState, RiskIncreaseDirection,
};
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExchangeObservationRevision(u64);

impl ExchangeObservationRevision {
    fn next(self) -> Self {
        Self(
            self.0
                .checked_add(1)
                .expect("exchange observation revision exhausted"),
        )
    }
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
    RiskExposureGated {
        gate: RiskExposureGateState,
    },
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
            Self::Running(ControlState::Automatic(
                AutoState::FollowingBand | AutoState::RiskExposureGated { .. },
            )) => TrackStatus::Active,
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
    pub position_qty: f64,
    pub market_data: Option<CurrentMarketData>,
    pub instrument_spec: InstrumentSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquisitionResetReason {
    FreshSessionStart,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AcquisitionSessionRuntimeView {
    pub acquisition_reset_reason: AcquisitionResetReason,
    pub acquisition_session_started_at: DateTime<Utc>,
    pub initial_frontier: Option<Exposure>,
    pub initial_release_units: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FreshAcquisitionSession {
    Pending {
        started_at: DateTime<Utc>,
    },
    Initialized {
        started_at: DateTime<Utc>,
        initial_frontier: Exposure,
        initial_release_units: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StrategyTargetView {
    pub risk_approved_target: Option<Exposure>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskAcquisitionRuntimeView {
    pub direction: RiskAcquisitionDirection,
    pub risk_approved_target: Exposure,
    pub acquisition_frontier: Exposure,
    pub backlog_units: f64,
    pub release_anchor_price: f64,
    pub release_anchor_target: Exposure,
    pub stale_release_elapsed_minutes: f64,
    pub stale_release_minutes: f64,
    pub next_advantage_target: Exposure,
    pub next_advantage_price: Option<f64>,
    pub next_release_units: f64,
    pub next_release_target: Exposure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskAcquisitionDirection {
    Long,
    Short,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InventoryDecisionSnapshot {
    pub observed_at: DateTime<Utc>,
    pub policy_target: Exposure,
    pub risk_approved_target: Exposure,
    pub acquisition_frontier: Option<Exposure>,
    pub execution_target: Exposure,
    pub reason: DecisionReason,
    pub applied_risk_cap: Option<AppliedRiskCap>,
    pub price_execution_gate: PriceExecutionGate,
    pub revision: u64,
    pub risk_acquisition: Option<RiskAcquisitionRuntimeView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InventoryDecisionView {
    pub revision: Option<u64>,
    pub policy_target: Option<f64>,
    pub risk_approved_target: Option<f64>,
    pub acquisition_frontier: Option<f64>,
    pub execution_target: Option<f64>,
    pub reason: DecisionReason,
}

impl InventoryDecisionView {
    fn committed(snapshot: &InventoryDecisionSnapshot) -> Self {
        Self {
            revision: Some(snapshot.revision),
            policy_target: Some(snapshot.policy_target.0),
            risk_approved_target: Some(snapshot.risk_approved_target.0),
            acquisition_frontier: snapshot.acquisition_frontier.as_ref().map(|value| value.0),
            execution_target: Some(snapshot.execution_target.0),
            reason: snapshot.reason,
        }
    }

    fn unavailable(reason: DecisionReason) -> Self {
        Self {
            revision: None,
            policy_target: None,
            risk_approved_target: None,
            acquisition_frontier: None,
            execution_target: None,
            reason,
        }
    }
}

impl Default for InventoryDecisionView {
    fn default() -> Self {
        Self::unavailable(DecisionReason::WaitingMarketData)
    }
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
    pub decision: InventoryDecisionView,
    pub acquisition_session: Option<AcquisitionSessionRuntimeView>,
    pub risk_acquisition: Option<RiskAcquisitionRuntimeView>,
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
    pub instrument_spec: InstrumentSpec,
    pub current_exposure: Exposure,
    pub position_qty: f64,
    pub signed_position_notional: Option<Amount>,
    pub contract_base_exposure: Option<Amount>,
    pub decision: InventoryDecisionView,
    pub acquisition_session: Option<AcquisitionSessionRuntimeView>,
    pub risk_acquisition: Option<RiskAcquisitionRuntimeView>,
    pub manual_target_override: Option<Exposure>,
    pub executor: ExecutorView,
    pub pnl_stats: TrackPnlStats,
    pub pnl: TrackPnlView,
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
    pub(crate) ledger_state: BoundaryLedgerState,
    pub(crate) bindings: Vec<LiveOrderBinding>,
    pub(crate) recent_terminal_orders: Vec<RecentTerminalOrder>,
    pub(crate) recovery_anomaly: Option<RecoveryAnomaly>,
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
                    quantity: binding.request.quantity.value(),
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

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRuntime {
    definition: TrackDefinition,
    // Process-local invalidation clock for composite REST snapshots. Candidate business state may
    // fail to commit, but this clock is retained separately by application and never moves back.
    exchange_observation_revision: ExchangeObservationRevision,
    pub(crate) instrument_spec: InstrumentSpec,
    pub(crate) track_state: TrackState,
    pub(crate) current_exposure: Exposure,
    pub(crate) current_position_qty: f64,
    // Reconcile owns desired_exposure; exchange sync/restore own observed order and risk fields.
    pub(crate) desired_exposure: Option<Exposure>,
    pub(crate) inventory_decision: Option<InventoryDecisionSnapshot>,
    pub(crate) inventory_decision_revision: u64,
    pub(crate) applied_risk_acquisition_release: Option<AppliedRiskAcquisitionRelease>,
    pub(crate) fresh_acquisition_session: Option<FreshAcquisitionSession>,
    pub(crate) executor_state: ExecutorState,
    pub(crate) pnl_stats: TrackPnlStats,
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

pub fn position_qty_per_exposure_unit(
    config: &TrackConfig,
    instrument_spec: &InstrumentSpec,
) -> f64 {
    let notional = Amount::new(
        config.notional_per_unit,
        instrument_spec.notional_asset().clone(),
    )
    .expect("validated track notional must be finite");
    instrument_spec
        .position_qty_for_notional(notional, config.band_center())
        .expect("validated track and instrument must define a position quantity")
        .value()
}

pub fn exposure_from_position_qty(
    config: &TrackConfig,
    instrument_spec: &InstrumentSpec,
    position_qty: f64,
) -> Exposure {
    let unit_qty = position_qty_per_exposure_unit(config, instrument_spec);
    if !unit_qty.is_finite() || unit_qty <= f64::EPSILON {
        Exposure(0.0)
    } else {
        Exposure(position_qty / unit_qty)
    }
}

pub fn position_notional(
    config: &TrackConfig,
    instrument_spec: &InstrumentSpec,
    position_qty: f64,
) -> f64 {
    instrument_spec
        .notional_from_position_qty(
            PositionQty::new(position_qty).expect("observed position quantity must be finite"),
            config.band_center(),
        )
        .expect("validated track and instrument must define position notional")
        .value()
}

pub fn required_additional_notional(
    config: &TrackConfig,
    instrument_spec: &InstrumentSpec,
    max_notional: f64,
    position_qty: f64,
) -> f64 {
    (max_notional - position_notional(config, instrument_spec, position_qty)).max(0.0)
}

fn current_position_values(
    instrument_spec: &InstrumentSpec,
    position_qty: f64,
    mark_price: Option<f64>,
    strategy_price: Option<f64>,
) -> (Option<Amount>, Option<Amount>) {
    let Ok(quantity) = PositionQty::new(position_qty) else {
        return (None, None);
    };
    let reference_price = mark_price
        .filter(|price| price.is_finite() && *price > f64::EPSILON)
        .or_else(|| strategy_price.filter(|price| price.is_finite() && *price > f64::EPSILON));
    let notional_price = match instrument_spec.position_basis() {
        PositionBasis::BaseAsset => reference_price,
        PositionBasis::Contracts { .. } => Some(reference_price.unwrap_or(1.0)),
    };
    let signed_notional = notional_price.and_then(|price| {
        instrument_spec
            .signed_notional_from_position_qty(quantity, price)
            .ok()
    });
    let contract_base_exposure = match instrument_spec.position_basis() {
        PositionBasis::BaseAsset => None,
        PositionBasis::Contracts { .. } => reference_price.and_then(|price| {
            instrument_spec
                .base_exposure_from_position_qty(quantity, price)
                .ok()
        }),
    };

    (signed_notional, contract_base_exposure)
}

impl TrackRuntime {
    pub fn new(
        definition: TrackDefinition,
        instrument_spec: InstrumentSpec,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self::with_tick_timeout_secs(
            definition.clone(),
            instrument_spec,
            started_at,
            definition.tick_timeout_secs(),
        )
    }

    pub fn with_tick_timeout_secs(
        definition: TrackDefinition,
        instrument_spec: InstrumentSpec,
        started_at: DateTime<Utc>,
        tick_timeout_secs: u64,
    ) -> Self {
        Self {
            definition,
            exchange_observation_revision: ExchangeObservationRevision::default(),
            instrument_spec,
            track_state: TrackState::WaitingMarketData,
            current_exposure: Exposure(0.0),
            current_position_qty: 0.0,
            desired_exposure: None,
            inventory_decision: None,
            inventory_decision_revision: 0,
            applied_risk_acquisition_release: None,
            fresh_acquisition_session: None,
            executor_state: ExecutorState::empty(started_at),
            pnl_stats: TrackPnlStats::default(),
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
        &self.definition.instrument().symbol
    }

    pub fn id(&self) -> &TrackId {
        self.definition.track_id()
    }

    pub fn instrument(&self) -> &Instrument {
        self.definition.instrument()
    }

    pub fn definition(&self) -> &TrackDefinition {
        &self.definition
    }

    pub fn config(&self) -> &TrackConfig {
        self.definition.track_config()
    }

    pub fn status(&self) -> TrackStatus {
        self.track_state.status()
    }

    pub fn manual_target_override(&self) -> Option<Exposure> {
        self.track_state.manual_target_override()
    }

    pub fn max_notional(&self) -> f64 {
        self.definition.max_notional()
    }

    pub fn loss_limits(&self) -> &LossLimits {
        self.definition.loss_limits()
    }

    #[cfg(test)]
    pub(crate) fn replace_definition_for_test(
        &mut self,
        config: TrackConfig,
        max_notional: f64,
        loss_limits: LossLimits,
    ) {
        self.definition = TrackDefinition::try_new(
            self.id().clone(),
            self.instrument().clone(),
            config,
            Some(max_notional),
            loss_limits,
            Some(self.tick_timeout_secs),
        )
        .expect("test definition replacement must be valid");
    }

    pub fn instrument_spec(&self) -> &InstrumentSpec {
        &self.instrument_spec
    }

    pub fn exchange_observation_revision(&self) -> ExchangeObservationRevision {
        self.exchange_observation_revision
    }

    pub(crate) fn advance_exchange_observation_revision(&mut self) {
        self.exchange_observation_revision = self.exchange_observation_revision.next();
    }

    pub(crate) fn set_exchange_observation_revision(
        &mut self,
        revision: ExchangeObservationRevision,
    ) {
        self.exchange_observation_revision = revision;
    }

    pub(crate) fn replace_executor_state(&mut self, executor_state: ExecutorState) {
        if self.executor_state != executor_state {
            self.executor_state = executor_state;
            self.advance_exchange_observation_revision();
        }
    }

    pub fn position_qty_per_exposure_unit(&self) -> f64 {
        position_qty_per_exposure_unit(self.config(), &self.instrument_spec)
    }

    pub fn exposure_from_position_qty(&self, position_qty: f64) -> Exposure {
        exposure_from_position_qty(self.config(), &self.instrument_spec, position_qty)
    }

    pub fn position_notional(&self, position_qty: f64) -> f64 {
        position_notional(self.config(), &self.instrument_spec, position_qty)
    }

    pub fn required_additional_notional(&self, position_qty: f64) -> f64 {
        required_additional_notional(
            self.config(),
            &self.instrument_spec,
            self.max_notional(),
            position_qty,
        )
    }

    fn static_definition(&self) -> TrackDefinition {
        TrackDefinition::try_new(
            self.id().clone(),
            self.instrument().clone(),
            self.config().clone(),
            Some(self.max_notional()),
            self.loss_limits().clone(),
            Some(self.tick_timeout_secs),
        )
        .expect("runtime static fields must remain a valid track definition")
    }

    pub fn fresh_start(
        &self,
        track_state: TrackState,
        pnl_stats: TrackPnlStats,
        external_inputs: FreshSessionExternalInputs,
        started_at: DateTime<Utc>,
    ) -> Self {
        let FreshSessionExternalInputs {
            current_exposure,
            position_qty,
            market_data,
            instrument_spec,
        } = external_inputs;
        let mut fresh = Self::with_tick_timeout_secs(
            self.static_definition(),
            instrument_spec,
            started_at,
            self.tick_timeout_secs,
        );
        fresh.exchange_observation_revision = self.exchange_observation_revision.next();
        fresh.track_state = track_state.clone();
        fresh.current_exposure = current_exposure.clone();
        fresh.current_position_qty = position_qty;
        fresh.desired_exposure = track_state.manual_target_override();
        fresh.fresh_acquisition_session = Some(FreshAcquisitionSession::Pending { started_at });
        fresh.executor_state =
            ExecutorState::empty(started_at).ensure_revision(fresh.config(), current_exposure);
        fresh.pnl_stats = pnl_stats;
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

        fresh.price_execution_gate = evaluate_price_execution_gate(
            PriceExecutionGate::Open,
            fresh.mark_price,
            match (fresh.best_bid, fresh.best_ask) {
                (Some(best_bid), Some(best_ask)) => Some(ExecutionQuote { best_bid, best_ask }),
                _ => None,
            },
        );
        fresh
    }

    pub fn fresh_acquisition_pending(&self) -> bool {
        matches!(
            self.fresh_acquisition_session,
            Some(FreshAcquisitionSession::Pending { .. })
        )
    }

    pub(crate) fn apply_fresh_acquisition_initialization(
        &mut self,
        initialization: Option<FreshAcquisitionInitialization>,
    ) {
        let Some(initialization) = initialization else {
            return;
        };
        let Some(FreshAcquisitionSession::Pending { started_at }) =
            self.fresh_acquisition_session.as_ref()
        else {
            debug_assert!(
                false,
                "fresh acquisition initialization requires a pending session"
            );
            return;
        };
        self.fresh_acquisition_session = Some(FreshAcquisitionSession::Initialized {
            started_at: *started_at,
            initial_frontier: initialization.initial_frontier,
            initial_release_units: initialization.initial_release_units,
        });
    }

    pub fn acquisition_session_view(&self) -> Option<AcquisitionSessionRuntimeView> {
        self.fresh_acquisition_session
            .as_ref()
            .map(|session| match session {
                FreshAcquisitionSession::Pending { started_at } => AcquisitionSessionRuntimeView {
                    acquisition_reset_reason: AcquisitionResetReason::FreshSessionStart,
                    acquisition_session_started_at: *started_at,
                    initial_frontier: None,
                    initial_release_units: None,
                },
                FreshAcquisitionSession::Initialized {
                    started_at,
                    initial_frontier,
                    initial_release_units,
                } => AcquisitionSessionRuntimeView {
                    acquisition_reset_reason: AcquisitionResetReason::FreshSessionStart,
                    acquisition_session_started_at: *started_at,
                    initial_frontier: Some(initial_frontier.clone()),
                    initial_release_units: Some(*initial_release_units),
                },
            })
    }

    pub fn inventory_decision(&self) -> Option<&InventoryDecisionSnapshot> {
        self.inventory_decision.as_ref()
    }

    pub fn runtime_state(&self) -> &TrackState {
        &self.track_state
    }

    pub fn inventory_decision_revision(&self) -> Option<u64> {
        self.inventory_decision
            .as_ref()
            .map(|decision| decision.revision)
    }

    pub fn market_data_stale(&self) -> bool {
        self.market_data_stale_since.is_some()
    }

    pub fn executor_view_changed(&self, next: &Self) -> bool {
        self.executor_state.view() != next.executor_state.view()
    }

    pub fn pnl_stats(&self) -> &TrackPnlStats {
        &self.pnl_stats
    }

    pub fn recovery_anomaly_active(&self) -> bool {
        self.executor_state.recovery_anomaly.is_some()
    }

    pub fn has_recorded_order_fact_for_client_order_id(&self, client_order_id: &str) -> bool {
        self.executor_state.bindings.iter().any(|binding| {
            binding.request.client_order_id == client_order_id && binding.order_id.is_some()
        }) || self
            .executor_state
            .recent_terminal_orders
            .iter()
            .any(|order| order.client_order_id == client_order_id)
    }

    pub fn has_absorbed_binding_for_cancel_receipt(
        &self,
        order_id: &str,
        receipt: &crate::exchange::OrderReceipt,
    ) -> bool {
        self.executor_state.bindings.iter().any(|binding| {
            binding.absorbed_exposure_qty > f64::EPSILON
                && (binding.order_id.as_deref() == Some(order_id)
                    || binding.order_id.as_deref() == Some(receipt.order_id.as_str())
                    || binding.request.client_order_id == receipt.client_order_id)
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_runtime_state(&mut self, runtime_state: TrackState) {
        self.track_state = runtime_state;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_risk_exposure_gate_state(
        &mut self,
        frontier: Exposure,
        anchor_price: f64,
        anchor_target: Exposure,
        stale_since: DateTime<Utc>,
    ) {
        self.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: RiskExposureGateState {
                    risk_release_frontier: frontier,
                    release_anchor_price: anchor_price,
                    release_anchor_target: anchor_target,
                    stale_since,
                },
            }));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn current_exposure(&self) -> &Exposure {
        &self.current_exposure
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn desired_exposure(&self) -> Option<&Exposure> {
        self.desired_exposure.as_ref()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_exposure_state(
        &mut self,
        current_exposure: Exposure,
        desired_exposure: Option<Exposure>,
    ) {
        self.current_exposure = current_exposure;
        self.desired_exposure = desired_exposure;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_pnl_stats(&mut self, pnl_stats: TrackPnlStats) {
        self.pnl_stats = pnl_stats;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_unrealized_pnl(&mut self, unrealized_pnl: f64) {
        self.risk_state.unrealized_pnl = unrealized_pnl;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn unrealized_pnl(&self) -> f64 {
        self.risk_state.unrealized_pnl
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_recovery_anomaly(&mut self, recovery_anomaly: Option<RecoveryAnomaly>) {
        self.executor_state.recovery_anomaly = recovery_anomaly;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn clear_executor_bindings(&mut self) {
        self.executor_state.bindings.clear();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn has_executor_bindings(&self) -> bool {
        !self.executor_state.bindings.is_empty()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn executor_ledger_anchor_exposure(&self) -> &Exposure {
        &self.executor_state.ledger_state.ledger_anchor_exposure
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_market_data_stale_since(&mut self, stale_since: Option<DateTime<Utc>>) {
        self.market_data_stale_since = stale_since;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn binding_receipt_for_client_order_id(
        &self,
        client_order_id: &str,
    ) -> Option<(Option<String>, BindingStatus)> {
        self.executor_state
            .bindings
            .iter()
            .find(|binding| binding.request.client_order_id == client_order_id)
            .map(|binding| (binding.order_id.clone(), binding.status))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_binding_order_status_for_client_order_id(
        &mut self,
        client_order_id: &str,
        order_id: Option<String>,
        status: BindingStatus,
    ) -> bool {
        let Some(binding) = self
            .executor_state
            .bindings
            .iter_mut()
            .find(|binding| binding.request.client_order_id == client_order_id)
        else {
            return false;
        };
        binding.order_id = order_id;
        binding.status = status;
        true
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn binding_is_active_for_client_order_id(&self, client_order_id: &str) -> Option<bool> {
        self.executor_state
            .bindings
            .iter()
            .find(|binding| binding.request.client_order_id == client_order_id)
            .map(|binding| binding.is_active())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn has_active_binding_for_order_id(&self, order_id: &str) -> bool {
        self.executor_state
            .bindings
            .iter()
            .any(|binding| binding.is_active() && binding.order_id.as_deref() == Some(order_id))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn first_executor_order_request(&self) -> Option<&crate::exchange::OrderRequest> {
        self.executor_state
            .bindings
            .first()
            .map(|binding| &binding.request)
    }

    pub(crate) fn next_inventory_decision_revision(&self) -> u64 {
        self.inventory_decision_revision
            .checked_add(1)
            .expect("inventory decision revision exhausted")
    }

    #[cfg(test)]
    pub(crate) fn apply_inventory_decision(&mut self, decision: Option<InventoryDecisionSnapshot>) {
        self.apply_inventory_decision_with_release(decision, None);
    }

    pub(crate) fn apply_inventory_decision_with_release(
        &mut self,
        decision: Option<InventoryDecisionSnapshot>,
        applied_release: Option<AppliedRiskAcquisitionRelease>,
    ) {
        assert!(
            decision.is_some() || applied_release.is_none(),
            "applied risk acquisition release requires an inventory decision"
        );
        if let Some(decision) = &decision {
            assert_eq!(
                decision.revision,
                self.next_inventory_decision_revision(),
                "inventory decision revision must advance exactly once"
            );
            self.inventory_decision_revision = decision.revision;
        }
        self.inventory_decision = decision;
        self.applied_risk_acquisition_release = applied_release;
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
            price_execution_gate: self.price_execution_gate,
        }
    }

    pub fn strategy_target_view(&self) -> StrategyTargetView {
        StrategyTargetView {
            risk_approved_target: self
                .publicly_committed_inventory_decision()
                .map(|decision| decision.risk_approved_target.clone()),
        }
    }

    pub fn inventory_decision_view(&self) -> InventoryDecisionView {
        if self.track_state.is_paused() {
            return InventoryDecisionView::unavailable(DecisionReason::Paused);
        }
        self.publicly_committed_inventory_decision()
            .map(InventoryDecisionView::committed)
            .unwrap_or_else(|| {
                InventoryDecisionView::unavailable(DecisionReason::WaitingMarketData)
            })
    }

    pub(crate) fn publicly_committed_inventory_decision(
        &self,
    ) -> Option<&InventoryDecisionSnapshot> {
        if matches!(self.track_state, TrackState::WaitingMarketData)
            || self.strategy_price_status == StrategyPriceStatus::Stale
            || self.market_data_stale_since.is_some()
        {
            return None;
        }
        self.inventory_decision.as_ref()
    }

    pub fn live_view(&self) -> TrackLiveView {
        let quote_health = self.quote_health_view();
        let committed = self.publicly_committed_inventory_decision();
        TrackLiveView {
            strategy_price: self.strategy_price,
            strategy_price_status: quote_health.strategy_price_status,
            mark_price: self.mark_price,
            best_bid: self.best_bid,
            best_ask: self.best_ask,
            decision: self.inventory_decision_view(),
            acquisition_session: self.acquisition_session_view(),
            risk_acquisition: committed.and_then(|decision| decision.risk_acquisition.clone()),
            price_execution_block_reason: match quote_health.price_execution_gate {
                PriceExecutionGate::Open => None,
                PriceExecutionGate::ManualRiskReductionOnly { reason }
                | PriceExecutionGate::NoSubmit { reason } => Some(reason),
            },
        }
    }

    pub fn runtime_view(&self) -> TrackRuntimeView {
        let live = self.live_view();
        let (signed_position_notional, contract_base_exposure) = current_position_values(
            &self.instrument_spec,
            self.current_position_qty,
            live.mark_price,
            live.strategy_price,
        );
        let pnl = self.pnl_stats.view(
            self.instrument_spec.settlement_asset(),
            self.risk_state.unrealized_pnl,
        );
        TrackRuntimeView {
            status: self.status(),
            instrument_spec: self.instrument_spec.clone(),
            current_exposure: self.current_exposure.clone(),
            position_qty: self.current_position_qty,
            signed_position_notional,
            contract_base_exposure,
            decision: live.decision,
            acquisition_session: live.acquisition_session,
            risk_acquisition: live.risk_acquisition,
            manual_target_override: self.manual_target_override(),
            executor: self.executor_state.view(),
            pnl_stats: self.pnl_stats.clone(),
            pnl,
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

    pub(crate) fn risk_acquisition_runtime_view(
        &self,
        target: &reconciler::TargetReconcileResult,
        strategy_price: f64,
        observed_at: DateTime<Utc>,
    ) -> Option<RiskAcquisitionRuntimeView> {
        let persisted_gate = gate_state_from_track_state(target.new_runtime_state.as_ref())
            .or_else(|| gate_state_from_track_state(Some(&self.track_state)));
        let transient_gate = match persisted_gate {
            Some(_) => None,
            None => {
                let acquisition_frontier = target.acquisition_frontier.as_ref()?;
                let backlog_units = target
                    .risk_approved_target
                    .delta(acquisition_frontier)
                    .0
                    .abs();
                if backlog_units <= f64::EPSILON {
                    return None;
                }
                Some(RiskExposureGateState {
                    risk_release_frontier: acquisition_frontier.clone(),
                    release_anchor_price: strategy_price,
                    release_anchor_target: acquisition_frontier.clone(),
                    stale_since: observed_at,
                })
            }
        };
        let gate = persisted_gate.or(transient_gate.as_ref())?;
        let release = target.risk_acquisition.clone().or_else(|| {
            risk_exposure_gate::pending_release(
                self.config().risk_acquisition,
                self.config().min_rebalance_units,
                gate,
                target.risk_approved_target.clone(),
            )
        })?;
        Some(self.risk_acquisition_view(
            &target.risk_approved_target,
            &gate.risk_release_frontier,
            gate,
            &release,
            observed_at,
        ))
    }

    fn risk_acquisition_view(
        &self,
        risk_approved_target: &Exposure,
        acquisition_frontier: &Exposure,
        gate: &RiskExposureGateState,
        release: &RiskAcquisitionRelease,
        observed_at: DateTime<Utc>,
    ) -> RiskAcquisitionRuntimeView {
        let next_advantage_price =
            trigger_price_for_boundary(release.advantage_target.0, self.config());
        let stale_release_elapsed_minutes = observed_at
            .signed_duration_since(gate.stale_since)
            .num_milliseconds()
            .max(0) as f64
            / 60_000.0;
        RiskAcquisitionRuntimeView {
            direction: RiskAcquisitionDirection::from(release.direction),
            risk_approved_target: risk_approved_target.clone(),
            acquisition_frontier: acquisition_frontier.clone(),
            backlog_units: risk_approved_target.delta(acquisition_frontier).0.abs(),
            release_anchor_price: gate.release_anchor_price,
            release_anchor_target: gate.release_anchor_target.clone(),
            stale_release_elapsed_minutes,
            stale_release_minutes: self.config().risk_acquisition.stale_release_minutes,
            next_advantage_target: release.advantage_target.clone(),
            next_advantage_price: next_advantage_price
                .is_finite()
                .then_some(next_advantage_price),
            next_release_units: release.release_units,
            next_release_target: release.release_target.clone(),
        }
    }
}

impl From<RiskIncreaseDirection> for RiskAcquisitionDirection {
    fn from(value: RiskIncreaseDirection) -> Self {
        match value {
            RiskIncreaseDirection::Long => Self::Long,
            RiskIncreaseDirection::Short => Self::Short,
        }
    }
}

fn gate_state_from_track_state(state: Option<&TrackState>) -> Option<&RiskExposureGateState> {
    match state {
        Some(TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
            gate,
        }))) => Some(gate),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::exchange::OrderRequest;
    use crate::executor::binding::{
        BindingPolicyState, BindingProposalKey, BindingStatus, LiveOrderBinding,
    };
    use crate::executor::boundary::{BoundaryDirection, BoundaryId, BoundaryOperation};
    use crate::executor::policy::PolicyKind;
    use crate::price_gate::SubmitPurpose;
    use poise_core::track::{TrackDefinition, Venue};

    #[test]
    fn inverse_contract_position_exposure_uses_contract_quantity() {
        let mut config = test_config();
        config.lower_price = 90_000.0;
        config.upper_price = 110_000.0;
        config.notional_per_unit = 1_000.0;
        let rules = inverse_rules();

        assert_eq!(position_qty_per_exposure_unit(&config, &rules), 10.0);
        assert_eq!(
            exposure_from_position_qty(&config, &rules, -30.0),
            Exposure(-3.0)
        );
        assert_eq!(position_notional(&config, &rules, -30.0), 3_000.0);

        config.lower_price = 40_000.0;
        config.upper_price = 60_000.0;
        assert_eq!(
            exposure_from_position_qty(&config, &rules, -30.0),
            Exposure(-3.0)
        );
    }

    #[test]
    fn runtime_view_values_linear_position_at_mark_price() {
        let mut runtime = test_runtime();
        runtime.current_position_qty = 2.0;
        runtime.strategy_price = Some(100.0);
        runtime.mark_price = Some(110.0);

        let view = runtime.runtime_view();

        let notional = view
            .signed_position_notional
            .expect("linear position should be valued from the mark price");
        assert_eq!(notional.value(), 220.0);
        assert_eq!(notional.asset(), runtime.instrument_spec.notional_asset());
        assert_eq!(view.contract_base_exposure, None);
    }

    #[test]
    fn runtime_view_values_inverse_contract_notional_and_base_exposure_once() {
        let mut runtime = TrackRuntime::new(test_definition(), inverse_rules(), Utc::now());
        runtime.current_position_qty = -30.0;
        runtime.strategy_price = Some(90_000.0);
        runtime.mark_price = Some(100_000.0);

        let view = runtime.runtime_view();

        let notional = view.signed_position_notional.unwrap();
        assert_eq!(notional.value(), -3_000.0);
        assert_eq!(notional.asset().as_str(), "USD");
        let base_exposure = view.contract_base_exposure.unwrap();
        assert_eq!(base_exposure.value(), -0.03);
        assert_eq!(base_exposure.asset().as_str(), "BTC");
    }

    #[test]
    fn runtime_clone_preserves_current_process_state() {
        let mut runtime = TrackRuntime::new(test_definition(), test_rules(0.1), Utc::now());
        runtime.executor_state = dirty_executor_state();

        let candidate = runtime.clone();
        assert!(
            !candidate.executor_state.ledger_state.progress.is_empty(),
            "runtime clones must preserve current-session progress"
        );
    }

    #[test]
    fn quote_health_view_projects_committed_price_gate_hysteresis() {
        let mut runtime = test_runtime();
        runtime.mark_price = Some(100.0);
        runtime.best_bid = Some(97.5);
        runtime.best_ask = Some(97.5);
        runtime.price_execution_gate = PriceExecutionGate::ManualRiskReductionOnly {
            reason: PriceExecutionBlockReason::MarkBookDivergence,
        };

        assert_eq!(
            runtime.quote_health_view().price_execution_gate,
            runtime.price_execution_gate,
            "read paths must project the committed hysteresis state"
        );
    }

    #[test]
    #[should_panic(expected = "inventory decision revision must advance exactly once")]
    fn inventory_decision_rejects_same_revision_replacement() {
        let mut runtime = test_runtime();
        let observed_at = Utc.with_ymd_and_hms(2026, 4, 23, 8, 0, 0).unwrap();
        commit_inventory_decision_for_test(&mut runtime, 95.0, observed_at);
        let mut replacement = runtime.inventory_decision().unwrap().clone();
        replacement.risk_approved_target = Exposure(9.0);

        runtime.apply_inventory_decision(Some(replacement));
    }

    #[test]
    fn live_view_exposes_risk_acquisition_backlog() {
        let mut config = test_config();
        config.risk_acquisition = poise_core::strategy::RiskAcquisitionConfig::default();
        let mut runtime = TrackRuntime::new(
            test_definition_with_config(config),
            test_rules(0.1),
            Utc::now(),
        );
        runtime.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::FollowingBand));
        runtime.current_exposure = Exposure(0.0);
        runtime.strategy_price = Some(95.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.mark_price = Some(95.0);
        runtime.best_bid = Some(94.9);
        runtime.best_ask = Some(95.1);
        commit_inventory_decision_for_test(
            &mut runtime,
            95.0,
            Utc.with_ymd_and_hms(2026, 4, 23, 8, 0, 0).unwrap(),
        );

        let live = runtime.live_view();
        assert!((live.decision.execution_target.unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(live.decision, runtime.runtime_view().decision);
        let risk_acquisition = live
            .risk_acquisition
            .expect("risk acquisition observability should be present while backlog exists");

        assert_eq!(risk_acquisition.direction, RiskAcquisitionDirection::Long);
        assert!((risk_acquisition.risk_approved_target.0 - 4.0).abs() < 1e-9);
        assert!((risk_acquisition.acquisition_frontier.0 - 2.0).abs() < 1e-9);
        assert!((risk_acquisition.backlog_units - 2.0).abs() < 1e-9);
        assert!((risk_acquisition.release_anchor_price - 95.0).abs() < 1e-9);
        assert!((risk_acquisition.release_anchor_target.0 - 4.0).abs() < 1e-9);
        assert!((risk_acquisition.next_advantage_target.0 - 6.0).abs() < 1e-9);
        assert_eq!(risk_acquisition.next_advantage_price, Some(92.5));
        assert!((risk_acquisition.next_release_units - 1.0).abs() < 1e-9);
        assert!((risk_acquisition.next_release_target.0 - 3.0).abs() < 1e-9);
    }

    #[test]
    fn decision_view_masks_old_snapshot_when_market_data_is_stale() {
        let mut runtime = test_runtime();
        runtime.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::FollowingBand));
        runtime.strategy_price = Some(95.0);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.best_bid = Some(94.9);
        runtime.best_ask = Some(95.1);
        commit_inventory_decision_for_test(
            &mut runtime,
            95.0,
            Utc.with_ymd_and_hms(2026, 4, 23, 8, 0, 0).unwrap(),
        );
        assert!(runtime.live_view().decision.revision.is_some());

        runtime.strategy_price_status = StrategyPriceStatus::Stale;
        let decision = runtime.live_view().decision;

        assert_eq!(decision.reason, DecisionReason::WaitingMarketData);
        assert_eq!(decision.revision, None);
        assert_eq!(decision.policy_target, None);
        assert_eq!(decision.risk_approved_target, None);
        assert_eq!(decision.acquisition_frontier, None);
        assert_eq!(decision.execution_target, None);

        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.track_state = TrackState::Paused {
            suspended: ControlState::Automatic(AutoState::FollowingBand),
        };
        let paused = runtime.runtime_view().decision;
        assert_eq!(paused.reason, DecisionReason::Paused);
        assert_eq!(paused.revision, None);
        assert_eq!(paused.policy_target, None);
        assert_eq!(paused.risk_approved_target, None);
        assert_eq!(paused.acquisition_frontier, None);
        assert_eq!(paused.execution_target, None);
    }

    #[test]
    fn live_view_exposes_cross_zero_risk_acquisition_backlog_without_bindings() {
        let mut runtime = TrackRuntime::new(
            test_definition_with_config(test_config()),
            test_rules(0.1),
            Utc::now(),
        );
        runtime.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::FollowingBand));
        runtime.current_exposure = Exposure(-0.25);
        runtime.strategy_price = Some(99.375);
        runtime.strategy_price_status = StrategyPriceStatus::Live;
        runtime.mark_price = Some(99.375);
        runtime.best_bid = Some(99.3);
        runtime.best_ask = Some(99.4);
        commit_inventory_decision_for_test(
            &mut runtime,
            99.375,
            Utc.with_ymd_and_hms(2026, 4, 23, 8, 0, 0).unwrap(),
        );

        let live = runtime.live_view();

        assert_eq!(live.decision.execution_target, Some(0.0));
        let risk_acquisition = live
            .risk_acquisition
            .expect("transient cross-zero risk acquisition backlog should be visible");

        assert_eq!(risk_acquisition.direction, RiskAcquisitionDirection::Long);
        assert!((risk_acquisition.risk_approved_target.0 - 0.5).abs() < 1e-9);
        assert_eq!(risk_acquisition.acquisition_frontier, Exposure(0.0));
        assert!((risk_acquisition.backlog_units - 0.5).abs() < 1e-9);
        assert!((risk_acquisition.release_anchor_price - 99.375).abs() < 1e-9);
        assert_eq!(risk_acquisition.release_anchor_target, Exposure(0.0));
        assert_eq!(risk_acquisition.next_advantage_target, Exposure(2.0));
        assert_eq!(risk_acquisition.next_advantage_price, Some(97.5));
        assert!((risk_acquisition.next_release_units - 0.5).abs() < 1e-9);
        assert_eq!(risk_acquisition.next_release_target, Exposure(0.5));
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
        commit_inventory_decision_for_test(
            &mut runtime,
            101.0,
            Utc.with_ymd_and_hms(2026, 4, 23, 8, 0, 0).unwrap(),
        );

        let fresh = runtime.fresh_start(
            TrackState::WaitingMarketData,
            TrackPnlStats {
                pnl_utc_day: Utc
                    .with_ymd_and_hms(2026, 4, 23, 0, 0, 0)
                    .unwrap()
                    .date_naive(),
                gross_realized_pnl_cumulative: 55.0,
                ..TrackPnlStats::default()
            },
            FreshSessionExternalInputs {
                current_exposure: Exposure(2.5),
                position_qty: 2.5,
                market_data: Some(CurrentMarketData {
                    strategy_price: 96.0,
                    mark_price: Some(95.8),
                    execution_quote: ExecutionQuote {
                        best_bid: 95.7,
                        best_ask: 95.9,
                    },
                    observed_at: Utc.with_ymd_and_hms(2026, 4, 23, 8, 1, 0).unwrap(),
                }),
                instrument_spec: test_rules(0.5),
            },
            Utc.with_ymd_and_hms(2026, 4, 23, 8, 2, 0).unwrap(),
        );

        assert_eq!(fresh.current_exposure, Exposure(2.5));
        assert_eq!(fresh.instrument_spec.price_tick(), 0.5);
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
        assert!(fresh.inventory_decision().is_none());
        assert_eq!(fresh.inventory_decision_revision, 0);
        assert_eq!(fresh.pnl_stats.gross_realized_pnl_cumulative, 55.0);
        assert_eq!(
            fresh.acquisition_session_view(),
            Some(AcquisitionSessionRuntimeView {
                acquisition_reset_reason: AcquisitionResetReason::FreshSessionStart,
                acquisition_session_started_at: Utc.with_ymd_and_hms(2026, 4, 23, 8, 2, 0).unwrap(),
                initial_frontier: None,
                initial_release_units: None,
            })
        );
    }

    #[test]
    fn acquisition_session_initialization_only_changes_candidate_runtime() {
        let started_at = Utc.with_ymd_and_hms(2026, 4, 23, 8, 2, 0).unwrap();
        let mut runtime = test_runtime().fresh_start(
            TrackState::WaitingMarketData,
            TrackPnlStats::default(),
            FreshSessionExternalInputs {
                current_exposure: Exposure(2.0),
                position_qty: 2.0,
                market_data: None,
                instrument_spec: test_rules(0.1),
            },
            started_at,
        );
        let pending_runtime = runtime.clone();

        runtime.apply_fresh_acquisition_initialization(Some(FreshAcquisitionInitialization {
            initial_frontier: Exposure(2.0),
            initial_release_units: 0.0,
        }));
        assert_eq!(
            runtime.acquisition_session_view(),
            Some(AcquisitionSessionRuntimeView {
                acquisition_reset_reason: AcquisitionResetReason::FreshSessionStart,
                acquisition_session_started_at: started_at,
                initial_frontier: Some(Exposure(2.0)),
                initial_release_units: Some(0.0),
            })
        );

        runtime = pending_runtime;

        assert!(runtime.fresh_acquisition_pending());
        assert_eq!(
            runtime.acquisition_session_view().unwrap().initial_frontier,
            None
        );
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
            TrackPnlStats::default(),
            FreshSessionExternalInputs {
                current_exposure: Exposure(1.0),
                position_qty: 1.0,
                market_data: None,
                instrument_spec: test_rules(1.0),
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
        assert_eq!(fresh.instrument_spec.price_tick(), 1.0);
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

    fn commit_inventory_decision_for_test(
        runtime: &mut TrackRuntime,
        strategy_price: f64,
        observed_at: DateTime<Utc>,
    ) {
        let target = reconciler::reconcile_target_at(runtime, strategy_price, observed_at);
        let decision = InventoryDecisionSnapshot {
            observed_at,
            policy_target: target.policy_target.clone(),
            risk_approved_target: target.risk_approved_target.clone(),
            acquisition_frontier: target.acquisition_frontier.clone(),
            execution_target: target.execution_target.clone(),
            reason: target.reason.unwrap_or(DecisionReason::SubmitPlanned),
            applied_risk_cap: target.applied_risk_cap.clone(),
            price_execution_gate: runtime.price_execution_gate,
            revision: runtime.next_inventory_decision_revision(),
            risk_acquisition: runtime.risk_acquisition_runtime_view(
                &target,
                strategy_price,
                observed_at,
            ),
        };
        runtime.apply_inventory_decision(Some(decision));
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
            risk_acquisition: Default::default(),
        }
    }

    fn test_loss_limits() -> LossLimits {
        LossLimits {
            daily_loss_limit: 1_000.0,
            total_loss_limit: 5_000.0,
        }
    }

    fn test_rules(price_tick: f64) -> InstrumentSpec {
        crate::test_instrument::linear_spec(price_tick, 0.01, 0.0, 0.0)
    }

    fn inverse_rules() -> InstrumentSpec {
        crate::test_instrument::inverse_spec(0.1, 1.0, 1.0, 0.0, 100.0)
    }

    fn test_definition() -> TrackDefinition {
        test_definition_with_config(test_config())
    }

    fn test_definition_with_config(config: TrackConfig) -> TrackDefinition {
        TrackDefinition::try_new(
            "test".into(),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            config,
            Some(10_000.0),
            test_loss_limits(),
            None,
        )
        .unwrap()
    }

    fn test_runtime() -> TrackRuntime {
        TrackRuntime::new(test_definition(), test_rules(0.1), Utc::now())
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
                quantity: PositionQty::new(1.0).unwrap(),
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
