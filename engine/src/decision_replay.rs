//! Deterministic evaluation of the real engine decision path.
//!
//! The runner drives [`TrackManager`] with timestamped observations and interprets its public
//! effects through a small deterministic exchange simulator. Reports expose stable semantic order
//! references instead of runtime-generated identities. The fill models are evaluation assumptions,
//! not predictions of queue position, latency, slippage, or real exchange fills.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use poise_core::replay::{estimate_settlement_fee, validate_replay_exchange_rules};
use poise_core::strategy::TrackConfig;
use poise_core::track::TrackDefinition;
use poise_core::types::{ExchangeRules, Exposure, Side};

use crate::execution_gate::added_risk_units;
use crate::execution_plan::TrackEffect;
use crate::executor::SubmitRecoveryResolution;
use crate::ledger::TrackPnlStats;
use crate::manager::{SubmitRecoveryOrderLookup, TrackManager};
use crate::observation::{
    MarketObservation, OrderObservation, PositionObservation, TrackObservation,
};
use crate::ports::{ClockPort, ExecutionQuote, OrderReceipt, OrderRequest, OrderStatus};
use crate::price_gate::SubmitPurpose;
use crate::risk_exposure_gate::AppliedRiskAcquisitionReleaseTrigger;
use crate::runtime::{
    FreshSessionExternalInputs, InventoryDecisionSnapshot, RiskAcquisitionDirection, TrackState,
    exposure_from_position_qty,
};
use crate::transition::TrackTransition;

const MAX_EFFECTS_PER_STEP: usize = 100_000;

/// Deterministic exchange-fill assumption used while replaying manager effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionReplayFillModel {
    /// Fully fills a new or retained order at its order price once it is marketable.
    ///
    /// A passive order remains working until a later execution quote crosses it. On each quote the
    /// manager first reconciles and processes cancel/replan effects; only orders still working are
    /// then eligible to fill. Every synthetic fill is estimated at the taker fee rate.
    ImmediateFullAtOrderPrice,
    /// Accepts submitted orders as working but never synthesizes a fill.
    NeverFill,
}

/// Why the risk-acquisition frontier was actively released in one decision revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionReplayReleaseTrigger {
    InitialRatio,
    PriceAdvantage,
    StaleTimeout,
}

/// Complete deterministic decision-replay scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionReplayInput {
    pub definition: TrackDefinition,
    pub exchange_rules: ExchangeRules,
    pub started_at: DateTime<Utc>,
    pub initial_position: PositionObservation,
    pub steps: Vec<DecisionScenarioStep>,
    pub fill_model: DecisionReplayFillModel,
}

/// One timestamped action in caller-provided order.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionScenarioStep {
    pub observed_at: DateTime<Utc>,
    pub action: DecisionScenarioAction,
}

/// Observation or fresh-session boundary applied by one scenario step.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionScenarioAction {
    Market(MarketObservation),
    Position(PositionObservation),
    Order(DecisionOrderObservation),
    Restart,
}

/// Exchange order fact addressed through the runner's stable order reference.
///
/// `filled_qty` is cumulative. A terminal `Filled` fact with zero reported quantity means the full
/// submitted quantity, matching the live executor's normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionOrderObservation {
    pub order_ref: String,
    pub filled_qty: f64,
    pub status: OrderStatus,
}

/// Deterministic aggregate and per-step decision-replay result.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionReplayReport {
    pub fill_model: DecisionReplayFillModel,
    pub steps: Vec<DecisionReplayStepReport>,
    pub final_simulated_position_qty: f64,
    pub final_simulated_exposure: Exposure,
    pub final_observed_position_qty: f64,
    pub final_observed_exposure: Exposure,
    pub total_estimated_taker_fee: f64,
    pub fee_asset: String,
}

/// Result after one action and all synchronous effects caused by that action.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionReplayStepReport {
    pub observed_at: DateTime<Utc>,
    pub action: DecisionScenarioAction,
    pub strategy_price: Option<f64>,
    pub committed_decision: Option<InventoryDecisionSnapshot>,
    pub effects: Vec<DecisionReplayEffect>,
    /// Signed native fill delta for this step: buys are positive and sells are negative.
    pub signed_filled_native_qty: f64,
    pub simulated_position_qty: f64,
    pub simulated_position_exposure: Exposure,
    pub observed_position_qty: f64,
    pub observed_position_exposure: Exposure,
    pub estimated_taker_fee: f64,
}

/// Stable semantic effect emitted by the deterministic exchange simulator.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionReplayEffect {
    SubmitOrder {
        order_ref: String,
        side: Side,
        price: f64,
        quantity: f64,
        reduce_only: bool,
        desired_exposure: Exposure,
        submit_purpose: SubmitPurpose,
    },
    /// Synthetic or explicitly observed cumulative fill delta.
    FillOrder {
        order_ref: String,
        side: Side,
        price: f64,
        quantity: f64,
        trade_notional: f64,
        risk_increase_native_qty: f64,
        position_before_qty: f64,
        position_after_qty: f64,
        estimated_taker_fee: f64,
    },
    RiskReleaseApplied {
        trigger: DecisionReplayReleaseTrigger,
        direction: RiskAcquisitionDirection,
        frontier_before: Exposure,
        frontier_after: Exposure,
        release_units: f64,
    },
    CancelOrder {
        order_ref: String,
    },
    CancelAll,
}

/// Runs one scenario through a fresh [`TrackManager`] and deterministic exchange simulator.
pub fn run_decision_replay(input: &DecisionReplayInput) -> Result<DecisionReplayReport> {
    DecisionReplayRunner::new(input)?.run(&input.steps)
}

#[derive(Debug)]
struct ReplayClock {
    now: Mutex<DateTime<Utc>>,
}

impl ReplayClock {
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    fn set(&self, now: DateTime<Utc>) {
        *self.now.lock().expect("decision replay clock poisoned") = now;
    }
}

impl ClockPort for ReplayClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("decision replay clock poisoned")
    }
}

struct DecisionReplayRunner {
    manager: TrackManager,
    clock: Arc<ReplayClock>,
    track_id: poise_core::track::TrackId,
    track_config: TrackConfig,
    exchange_rules: ExchangeRules,
    fill_model: DecisionReplayFillModel,
    last_observed_at: DateTime<Utc>,
    simulated_position_qty: f64,
    simulated_unrealized_pnl: f64,
    total_estimated_taker_fee: f64,
    execution_quote: Option<ExecutionQuote>,
    session: u64,
    next_order_number: u64,
    orders: Vec<SimulatedOrder>,
    last_reported_release_key: Option<(u64, u64)>,
}

#[derive(Debug, Clone)]
struct SimulatedOrder {
    order_ref: String,
    session: u64,
    exchange_order_id: String,
    request: OrderRequest,
    filled_qty: f64,
    status: OrderStatus,
}

#[derive(Default)]
struct StepAccumulator {
    effects: Vec<DecisionReplayEffect>,
    signed_filled_native_qty: f64,
    estimated_taker_fee: f64,
}

impl DecisionReplayRunner {
    fn new(input: &DecisionReplayInput) -> Result<Self> {
        validate_position(&input.initial_position).context("invalid initial position")?;
        validate_replay_exchange_rules(&input.exchange_rules).map_err(anyhow::Error::msg)?;
        let unit_qty = input.exchange_rules.native_qty_per_exposure_unit(
            input.definition.track_config().notional_per_unit,
            input.definition.track_config().band_center(),
        );
        ensure!(
            unit_qty.is_finite() && unit_qty > f64::EPSILON,
            "native quantity per exposure unit must be finite and positive"
        );
        let clock = Arc::new(ReplayClock::new(input.started_at));
        let mut manager = TrackManager::new(clock.clone());
        let track_id = input.definition.track_id().clone();
        let track_config = input.definition.track_config().clone();
        manager
            .add_track(input.definition.clone(), input.exchange_rules.clone())
            .context("add decision replay track")?;
        let initial_exposure = exposure_from_position_qty(
            &track_config,
            &input.exchange_rules,
            input.initial_position.qty,
        );
        manager
            .fresh_start_track(
                &track_id,
                TrackState::WaitingMarketData,
                TrackPnlStats::default(),
                FreshSessionExternalInputs {
                    current_exposure: initial_exposure,
                    position_qty: input.initial_position.qty,
                    market_data: None,
                    exchange_rules: input.exchange_rules.clone(),
                },
            )
            .context("fresh-start decision replay track")?;
        let initial_transition = manager
            .observe(
                &track_id,
                TrackObservation::Position(input.initial_position.clone()),
            )
            .context("observe initial decision replay position")?;
        ensure!(
            initial_transition.effects.is_empty(),
            "initial position produced execution effects before market data"
        );

        Ok(Self {
            manager,
            clock,
            track_id,
            track_config,
            exchange_rules: input.exchange_rules.clone(),
            fill_model: input.fill_model,
            last_observed_at: input.started_at,
            simulated_position_qty: input.initial_position.qty,
            simulated_unrealized_pnl: input.initial_position.unrealized_pnl,
            total_estimated_taker_fee: 0.0,
            execution_quote: None,
            session: 1,
            next_order_number: 1,
            orders: Vec::new(),
            last_reported_release_key: None,
        })
    }

    fn run(mut self, steps: &[DecisionScenarioStep]) -> Result<DecisionReplayReport> {
        let mut reports = Vec::with_capacity(steps.len());
        for (index, step) in steps.iter().enumerate() {
            ensure!(
                step.observed_at >= self.last_observed_at,
                "decision replay step {index} timestamp must not move backwards"
            );
            self.clock.set(step.observed_at);
            self.last_observed_at = step.observed_at;
            let report = self.apply_step(step).map_err(|error| {
                anyhow::anyhow!("decision replay step {index} failed: {error:#}")
            })?;
            reports.push(report);
        }

        let observed = self
            .manager
            .track_runtime_view(&self.track_id)
            .context("load final decision replay runtime")?;
        Ok(DecisionReplayReport {
            fill_model: self.fill_model,
            steps: reports,
            final_simulated_position_qty: self.simulated_position_qty,
            final_simulated_exposure: self.simulated_exposure(),
            final_observed_position_qty: observed.position_qty,
            final_observed_exposure: observed.current_exposure,
            total_estimated_taker_fee: self.total_estimated_taker_fee,
            fee_asset: self.exchange_rules.settlement_asset.clone(),
        })
    }

    fn apply_step(&mut self, step: &DecisionScenarioStep) -> Result<DecisionReplayStepReport> {
        let mut accumulator = StepAccumulator::default();
        let effects = match &step.action {
            DecisionScenarioAction::Market(observation) => {
                validate_market(observation)?;
                if let MarketObservation::ExecutionQuote { execution_quote } = observation {
                    self.execution_quote = Some(*execution_quote);
                }
                let transition = self.manager.observe(
                    &self.track_id,
                    TrackObservation::Market(observation.clone()),
                )?;
                self.collect_transition_release(&transition, &mut accumulator);
                transition.effects
            }
            DecisionScenarioAction::Position(observation) => {
                validate_position(observation)?;
                self.simulated_position_qty = observation.qty;
                self.simulated_unrealized_pnl = observation.unrealized_pnl;
                let transition = self.manager.observe(
                    &self.track_id,
                    TrackObservation::Position(observation.clone()),
                )?;
                self.collect_transition_release(&transition, &mut accumulator);
                transition.effects
            }
            DecisionScenarioAction::Order(observation) => {
                self.apply_explicit_order_observation(observation, &mut accumulator)?
            }
            DecisionScenarioAction::Restart => {
                self.restart()?;
                Vec::new()
            }
        };
        self.drain_effects(effects, &mut accumulator)?;
        if matches!(
            &step.action,
            DecisionScenarioAction::Market(MarketObservation::ExecutionQuote { .. })
        ) && matches!(
            self.fill_model,
            DecisionReplayFillModel::ImmediateFullAtOrderPrice
        ) {
            self.fill_retained_marketable_orders(&mut accumulator)?;
        }

        let observed = self.manager.track_runtime_view(&self.track_id)?;
        let committed_decision = self
            .manager
            .get_track(self.track_id.as_str())
            .and_then(|track| track.publicly_committed_inventory_decision().cloned());
        Ok(DecisionReplayStepReport {
            observed_at: step.observed_at,
            action: step.action.clone(),
            strategy_price: observed.strategy_price,
            committed_decision,
            effects: accumulator.effects,
            signed_filled_native_qty: accumulator.signed_filled_native_qty,
            simulated_position_qty: self.simulated_position_qty,
            simulated_position_exposure: self.simulated_exposure(),
            observed_position_qty: observed.position_qty,
            observed_position_exposure: observed.current_exposure,
            estimated_taker_fee: accumulator.estimated_taker_fee,
        })
    }

    fn collect_transition_release(
        &mut self,
        transition: &TrackTransition,
        accumulator: &mut StepAccumulator,
    ) {
        let Some(decision) = transition.next_state().inventory_decision.as_ref() else {
            return;
        };
        let Some(release) = transition
            .next_state()
            .applied_risk_acquisition_release
            .as_ref()
        else {
            return;
        };
        let key = (self.session, decision.revision);
        if self.last_reported_release_key == Some(key) {
            return;
        }
        self.last_reported_release_key = Some(key);
        let trigger = match release.trigger {
            AppliedRiskAcquisitionReleaseTrigger::InitialRatio => {
                DecisionReplayReleaseTrigger::InitialRatio
            }
            AppliedRiskAcquisitionReleaseTrigger::PriceAdvantage => {
                DecisionReplayReleaseTrigger::PriceAdvantage
            }
            AppliedRiskAcquisitionReleaseTrigger::StaleTimeout => {
                DecisionReplayReleaseTrigger::StaleTimeout
            }
        };
        accumulator
            .effects
            .push(DecisionReplayEffect::RiskReleaseApplied {
                trigger,
                direction: RiskAcquisitionDirection::from(release.direction),
                frontier_before: release.frontier_before.clone(),
                frontier_after: release.frontier_after.clone(),
                release_units: release.release_units,
            });
    }

    fn drain_effects(
        &mut self,
        effects: Vec<TrackEffect>,
        accumulator: &mut StepAccumulator,
    ) -> Result<()> {
        let mut queue = VecDeque::from(effects);
        let mut processed = 0usize;
        while let Some(effect) = queue.pop_front() {
            processed += 1;
            ensure!(
                processed <= MAX_EFFECTS_PER_STEP,
                "decision replay exceeded {MAX_EFFECTS_PER_STEP} effects in one step at simulated position {} after {:?}",
                self.simulated_position_qty,
                accumulator.effects.last()
            );
            match effect {
                TrackEffect::SubmitOrder {
                    submit_purpose,
                    recovery_token,
                    ..
                } => {
                    let plan = self.manager.recover_submit_effect(
                        &self.track_id,
                        &recovery_token,
                        &SubmitRecoveryOrderLookup::NotQueried,
                    )?;
                    queue.extend(plan.effects);
                    if let SubmitRecoveryResolution::Proceed {
                        request,
                        desired_exposure,
                        ..
                    } = plan.resolution
                    {
                        self.apply_submit(
                            request,
                            desired_exposure,
                            submit_purpose,
                            accumulator,
                            &mut queue,
                        )?;
                    }
                }
                TrackEffect::CancelOrder { order_id, .. } => {
                    self.apply_cancel(&order_id, accumulator)?;
                }
                TrackEffect::CancelAll { .. } => {
                    self.manager.record_cancel_all_success(&self.track_id)?;
                    for order in self.orders.iter_mut().filter(|order| {
                        order.session == self.session && order.status.keeps_working_order()
                    }) {
                        order.status = OrderStatus::Canceled;
                    }
                    accumulator.effects.push(DecisionReplayEffect::CancelAll);
                }
            }
        }
        Ok(())
    }

    fn apply_submit(
        &mut self,
        request: OrderRequest,
        desired_exposure: Exposure,
        submit_purpose: SubmitPurpose,
        accumulator: &mut StepAccumulator,
        queue: &mut VecDeque<TrackEffect>,
    ) -> Result<()> {
        let order_number = self.next_order_number;
        self.next_order_number = self
            .next_order_number
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("decision replay order sequence exhausted"))?;
        let order_ref = format!("order-{order_number}");
        let exchange_order_id = format!("decision-replay-order-{order_number}");
        let (filled_qty, status) = match self.fill_model {
            DecisionReplayFillModel::ImmediateFullAtOrderPrice
                if self.order_is_marketable(&request) =>
            {
                (request.quantity, OrderStatus::Filled)
            }
            DecisionReplayFillModel::ImmediateFullAtOrderPrice
            | DecisionReplayFillModel::NeverFill => (0.0, OrderStatus::New),
        };
        let receipt = OrderReceipt {
            order_id: exchange_order_id.clone(),
            client_order_id: request.client_order_id.clone(),
            filled_qty,
            status,
        };

        accumulator.effects.push(DecisionReplayEffect::SubmitOrder {
            order_ref: order_ref.clone(),
            side: request.side,
            price: request.price,
            quantity: request.quantity,
            reduce_only: request.reduce_only,
            desired_exposure: desired_exposure.clone(),
            submit_purpose,
        });
        self.orders.push(SimulatedOrder {
            order_ref: order_ref.clone(),
            session: self.session,
            exchange_order_id,
            request: request.clone(),
            filled_qty,
            status,
        });

        let receipt_transition = self.manager.record_submit_receipt(
            &self.track_id,
            &request,
            desired_exposure,
            &receipt,
        )?;
        self.collect_transition_release(&receipt_transition, accumulator);
        queue.extend(receipt_transition.effects);

        if status == OrderStatus::Filled {
            self.apply_simulated_fill(
                &order_ref,
                request.side,
                request.quantity,
                request.price,
                accumulator,
            )?;
            let position_transition = self.manager.observe(
                &self.track_id,
                TrackObservation::Position(PositionObservation {
                    qty: self.simulated_position_qty,
                    unrealized_pnl: self.simulated_unrealized_pnl,
                }),
            )?;
            self.collect_transition_release(&position_transition, accumulator);
            queue.extend(position_transition.effects);
        }
        Ok(())
    }

    fn fill_retained_marketable_orders(&mut self, accumulator: &mut StepAccumulator) -> Result<()> {
        let mut filled_orders = 0usize;
        loop {
            let Some(index) = self.orders.iter().position(|order| {
                order.session == self.session
                    && order.status.keeps_working_order()
                    && self.order_is_marketable(&order.request)
            }) else {
                return Ok(());
            };
            filled_orders += 1;
            ensure!(
                filled_orders <= MAX_EFFECTS_PER_STEP,
                "decision replay exceeded {MAX_EFFECTS_PER_STEP} retained fills in one step"
            );
            let effects = self.fill_working_order(index, accumulator)?;
            self.drain_effects(effects, accumulator)?;
        }
    }

    fn fill_working_order(
        &mut self,
        index: usize,
        accumulator: &mut StepAccumulator,
    ) -> Result<Vec<TrackEffect>> {
        let order = &self.orders[index];
        let fill_delta = (order.request.quantity - order.filled_qty).max(0.0);
        let order_ref = order.order_ref.clone();
        let side = order.request.side;
        let price = order.request.price;
        let order_observation = OrderObservation {
            order_id: order.exchange_order_id.clone(),
            client_order_id: order.request.client_order_id.clone(),
            side,
            price,
            quantity: order.request.quantity,
            filled_qty: order.request.quantity,
            status: OrderStatus::Filled,
        };
        self.orders[index].filled_qty = self.orders[index].request.quantity;
        self.orders[index].status = OrderStatus::Filled;

        let order_transition = self
            .manager
            .observe(&self.track_id, TrackObservation::Order(order_observation))?;
        self.collect_transition_release(&order_transition, accumulator);
        if fill_delta > f64::EPSILON {
            self.apply_simulated_fill(&order_ref, side, fill_delta, price, accumulator)?;
        }
        let position_transition = self.manager.observe(
            &self.track_id,
            TrackObservation::Position(PositionObservation {
                qty: self.simulated_position_qty,
                unrealized_pnl: self.simulated_unrealized_pnl,
            }),
        )?;
        self.collect_transition_release(&position_transition, accumulator);
        let mut effects = order_transition.effects;
        effects.extend(position_transition.effects);
        Ok(effects)
    }

    fn apply_cancel(
        &mut self,
        exchange_order_id: &str,
        accumulator: &mut StepAccumulator,
    ) -> Result<()> {
        let index = self
            .orders
            .iter()
            .position(|order| {
                order.session == self.session
                    && order.exchange_order_id == exchange_order_id
                    && order.status.keeps_working_order()
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cancel references unknown current-session order `{exchange_order_id}`"
                )
            })?;
        let order_ref = self.orders[index].order_ref.clone();
        let receipt = OrderReceipt {
            order_id: exchange_order_id.to_string(),
            client_order_id: self.orders[index].request.client_order_id.clone(),
            filled_qty: self.orders[index].filled_qty,
            status: OrderStatus::Canceled,
        };
        self.manager
            .record_cancel_order_success(&self.track_id, exchange_order_id, &receipt)?;
        self.orders[index].status = OrderStatus::Canceled;
        accumulator
            .effects
            .push(DecisionReplayEffect::CancelOrder { order_ref });
        Ok(())
    }

    fn apply_explicit_order_observation(
        &mut self,
        observation: &DecisionOrderObservation,
        accumulator: &mut StepAccumulator,
    ) -> Result<Vec<TrackEffect>> {
        ensure!(
            !observation.order_ref.trim().is_empty(),
            "order_ref must not be empty"
        );
        ensure!(
            observation.filled_qty.is_finite() && observation.filled_qty >= 0.0,
            "filled_qty must be finite and non-negative"
        );
        let index = self
            .orders
            .iter()
            .position(|order| {
                order.session == self.session && order.order_ref == observation.order_ref
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown current-session order_ref `{}`",
                    observation.order_ref
                )
            })?;
        let order = &self.orders[index];
        let effective_filled_qty = observation
            .status
            .effective_cumulative_filled_qty(observation.filled_qty, order.request.quantity);
        ensure!(
            effective_filled_qty + f64::EPSILON >= order.filled_qty,
            "filled_qty for `{}` must not move backwards from {} to {}",
            observation.order_ref,
            order.filled_qty,
            effective_filled_qty
        );
        ensure!(
            effective_filled_qty <= order.request.quantity + f64::EPSILON,
            "filled_qty for `{}` exceeds order quantity {}",
            observation.order_ref,
            order.request.quantity
        );
        let previous_filled_qty = order.filled_qty;
        let previous_status = order.status;
        let side = order.request.side;
        let price = order.request.price;
        let order_observation = OrderObservation {
            order_id: order.exchange_order_id.clone(),
            client_order_id: order.request.client_order_id.clone(),
            side,
            price,
            quantity: order.request.quantity,
            filled_qty: observation.filled_qty,
            status: observation.status,
        };
        let (transition, _absorb_result) = self
            .manager
            .observe_order_update(&self.track_id, order_observation)?;
        self.collect_transition_release(&transition, accumulator);
        // Manager absorption and exchange truth are different facts. A late fill can arrive after
        // the manager has already pruned a canceled binding, so simulator position/fee deltas use
        // the stable order's own cumulative baseline even when the manager reports a duplicate.
        let fill_delta = (effective_filled_qty - previous_filled_qty).max(0.0);
        self.orders[index].filled_qty = effective_filled_qty;
        if !previous_status.clears_working_order() || observation.status.clears_working_order() {
            self.orders[index].status = observation.status;
        }
        if fill_delta > f64::EPSILON {
            self.apply_simulated_fill(
                &observation.order_ref,
                side,
                fill_delta,
                price,
                accumulator,
            )?;
        }
        Ok(transition.effects)
    }

    fn apply_simulated_fill(
        &mut self,
        order_ref: &str,
        side: Side,
        quantity: f64,
        price: f64,
        accumulator: &mut StepAccumulator,
    ) -> Result<()> {
        let signed_quantity = match side {
            Side::Buy => quantity,
            Side::Sell => -quantity,
        };
        let position_before_qty = self.simulated_position_qty;
        let position_after_qty = position_before_qty + signed_quantity;
        let risk_increase_native_qty = added_risk_units(position_before_qty, position_after_qty);
        self.simulated_position_qty = position_after_qty;
        accumulator.signed_filled_native_qty += signed_quantity;
        let trade_notional = self
            .exchange_rules
            .notional_from_native_qty(quantity, price);
        let fee = estimate_settlement_fee(
            &self.exchange_rules,
            trade_notional,
            price,
            self.exchange_rules.taker_fee_rate,
        )
        .map_err(anyhow::Error::msg)?;
        accumulator.estimated_taker_fee += fee;
        self.total_estimated_taker_fee += fee;
        accumulator.effects.push(DecisionReplayEffect::FillOrder {
            order_ref: order_ref.to_string(),
            side,
            price,
            quantity,
            trade_notional,
            risk_increase_native_qty,
            position_before_qty,
            position_after_qty,
            estimated_taker_fee: fee,
        });
        Ok(())
    }

    fn restart(&mut self) -> Result<()> {
        let pnl_stats = self.manager.track_runtime_view(&self.track_id)?.pnl_stats;
        self.session = self
            .session
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("decision replay session sequence exhausted"))?;
        self.execution_quote = None;
        self.manager.fresh_start_track(
            &self.track_id,
            TrackState::WaitingMarketData,
            pnl_stats,
            FreshSessionExternalInputs {
                current_exposure: self.simulated_exposure(),
                position_qty: self.simulated_position_qty,
                market_data: None,
                exchange_rules: self.exchange_rules.clone(),
            },
        )?;
        let transition = self.manager.observe(
            &self.track_id,
            TrackObservation::Position(PositionObservation {
                qty: self.simulated_position_qty,
                unrealized_pnl: self.simulated_unrealized_pnl,
            }),
        )?;
        ensure!(
            transition.effects.is_empty(),
            "restart position produced execution effects before market data"
        );
        Ok(())
    }

    fn simulated_exposure(&self) -> Exposure {
        exposure_from_position_qty(
            &self.track_config,
            &self.exchange_rules,
            self.simulated_position_qty,
        )
    }

    fn order_is_marketable(&self, request: &OrderRequest) -> bool {
        let Some(quote) = self.execution_quote else {
            return false;
        };
        match request.side {
            Side::Buy => request.price + f64::EPSILON >= quote.best_ask,
            Side::Sell => request.price <= quote.best_bid + f64::EPSILON,
        }
    }
}

fn validate_position(position: &PositionObservation) -> Result<()> {
    ensure!(position.qty.is_finite(), "position qty must be finite");
    ensure!(
        position.unrealized_pnl.is_finite(),
        "position unrealized_pnl must be finite"
    );
    Ok(())
}

fn validate_market(market: &MarketObservation) -> Result<()> {
    match market {
        MarketObservation::ExecutionQuote { execution_quote } => {
            ensure!(
                execution_quote.best_bid.is_finite() && execution_quote.best_bid > 0.0,
                "best_bid must be finite and positive"
            );
            ensure!(
                execution_quote.best_ask.is_finite() && execution_quote.best_ask > 0.0,
                "best_ask must be finite and positive"
            );
            ensure!(
                execution_quote.best_bid <= execution_quote.best_ask,
                "best_bid must not exceed best_ask"
            );
        }
        MarketObservation::MarkPrice { mark_price } => {
            ensure!(
                mark_price.is_finite() && *mark_price > 0.0,
                "mark_price must be finite and positive"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_core::types::{ExchangeRules, Exposure, QuantityKind};

    use super::*;
    use crate::execution_plan::DecisionReason;
    use crate::observation::MarketObservation;
    use crate::ports::ExecutionQuote;

    #[test]
    fn decision_replay_is_stable_and_normalizes_runtime_order_identity() {
        let input = replay_input(0.0, DecisionReplayFillModel::ImmediateFullAtOrderPrice);

        let first = run_decision_replay(&input).unwrap();
        let second = run_decision_replay(&input).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.steps[0].action, input.steps[0].action);
        let order_refs = first
            .steps
            .iter()
            .flat_map(|step| &step.effects)
            .filter_map(|effect| match effect {
                DecisionReplayEffect::SubmitOrder { order_ref, .. } => Some(order_ref.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!order_refs.is_empty());
        assert_eq!(order_refs[0], "order-1");
        assert!(
            order_refs
                .iter()
                .all(|order_ref| order_ref.starts_with("order-"))
        );
        assert_eq!(first.fill_model, input.fill_model);
        assert_eq!(first.steps[0].strategy_price, Some(95.0));
    }

    #[test]
    fn decision_replay_preserves_one_applied_release_across_synchronous_fill_revisions() {
        for fill_model in [
            DecisionReplayFillModel::ImmediateFullAtOrderPrice,
            DecisionReplayFillModel::NeverFill,
        ] {
            let report = run_decision_replay(&replay_input(0.0, fill_model)).unwrap();
            let releases = report.steps[1]
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    DecisionReplayEffect::RiskReleaseApplied {
                        trigger,
                        direction,
                        frontier_before,
                        frontier_after,
                        release_units,
                    } => Some((
                        *trigger,
                        *direction,
                        frontier_before.0,
                        frontier_after.0,
                        *release_units,
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();

            assert_eq!(
                releases,
                vec![(
                    DecisionReplayReleaseTrigger::PriceAdvantage,
                    crate::runtime::RiskAcquisitionDirection::Long,
                    0.0,
                    1.5,
                    1.5,
                )]
            );
        }
    }

    #[test]
    fn decision_replay_release_revision_dedup_is_scoped_to_restart_session() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.steps.extend([
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::minutes(2),
                action: DecisionScenarioAction::Restart,
            },
            market_step(input.started_at + Duration::minutes(2), 95.0),
            market_step(input.started_at + Duration::minutes(3), 92.5),
        ]);

        let report = run_decision_replay(&input).unwrap();
        let releases = report
            .steps
            .iter()
            .flat_map(|step| &step.effects)
            .filter(|effect| matches!(effect, DecisionReplayEffect::RiskReleaseApplied { .. }))
            .count();

        assert_eq!(releases, 2);
        assert!(report.steps[2].committed_decision.is_none());
        assert!(
            !report.steps[2]
                .effects
                .iter()
                .any(|effect| matches!(effect, DecisionReplayEffect::RiskReleaseApplied { .. }))
        );
    }

    #[test]
    fn decision_replay_preserves_caller_order_for_equal_timestamps() {
        let started_at = at(2026, 7, 1, 9, 0);
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.steps = vec![
            market_step(started_at, 95.0),
            DecisionScenarioStep {
                observed_at: started_at,
                action: DecisionScenarioAction::Market(MarketObservation::MarkPrice {
                    mark_price: 95.0,
                }),
            },
            DecisionScenarioStep {
                observed_at: started_at,
                action: DecisionScenarioAction::Position(PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                }),
            },
        ];

        let report = run_decision_replay(&input).unwrap();

        assert_eq!(
            report
                .steps
                .iter()
                .map(|step| step.action.clone())
                .collect::<Vec<_>>(),
            input
                .steps
                .iter()
                .map(|step| step.action.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn decision_replay_fill_models_change_only_simulated_execution_facts() {
        let immediate = run_decision_replay(&replay_input(
            0.0,
            DecisionReplayFillModel::ImmediateFullAtOrderPrice,
        ))
        .unwrap();
        let never =
            run_decision_replay(&replay_input(0.0, DecisionReplayFillModel::NeverFill)).unwrap();

        assert!(immediate.final_simulated_position_qty > 0.0);
        assert!(immediate.total_estimated_taker_fee > 0.0);
        assert_eq!(never.final_simulated_position_qty, 0.0);
        assert_eq!(never.total_estimated_taker_fee, 0.0);
        assert_eq!(immediate.fee_asset, "USDT");
        assert_eq!(never.fee_asset, "USDT");
    }

    #[test]
    fn decision_replay_initial_nonzero_position_anchors_fresh_executor_state() {
        let mut input = replay_input(2.0, DecisionReplayFillModel::NeverFill);
        input.steps = vec![market_step(input.started_at, 92.5)];

        let report = run_decision_replay(&input).unwrap();

        let decision = report.steps[0].committed_decision.as_ref().unwrap();
        assert_eq!(decision.risk_approved_target, Exposure(6.0));
        assert_eq!(decision.execution_target, Exposure(2.0));
        assert_ne!(decision.reason, DecisionReason::RecoveryRequired);
        assert!(!report.steps[0].effects.is_empty());
        assert!(report.steps[0].effects.iter().all(|effect| matches!(
            effect,
            DecisionReplayEffect::SubmitOrder {
                reduce_only: true,
                ..
            }
        )));
    }

    #[test]
    fn decision_replay_restart_discards_old_frontier_and_offline_stale_time() {
        let started_at = at(2026, 7, 1, 9, 0);
        let restart_at = started_at + Duration::days(31);
        let mut input = replay_input(2.0, DecisionReplayFillModel::NeverFill);
        input.steps = vec![
            market_step(started_at, 92.5),
            market_step(started_at + Duration::minutes(60), 92.5),
            DecisionScenarioStep {
                observed_at: restart_at,
                action: DecisionScenarioAction::Restart,
            },
            market_step(restart_at, 92.5),
            market_step(restart_at + Duration::minutes(59), 92.5),
            market_step(restart_at + Duration::minutes(60), 92.5),
        ];

        let report = run_decision_replay(&input).unwrap();

        assert!(
            report.steps[1]
                .committed_decision
                .as_ref()
                .unwrap()
                .execution_target
                .0
                > 2.0
        );
        assert!(report.steps[2].committed_decision.is_none());
        assert_eq!(
            report.steps[3]
                .committed_decision
                .as_ref()
                .unwrap()
                .execution_target,
            Exposure(2.0)
        );
        assert_eq!(
            report.steps[4]
                .committed_decision
                .as_ref()
                .unwrap()
                .execution_target,
            Exposure(2.0)
        );
        assert!(
            report.steps[5]
                .committed_decision
                .as_ref()
                .unwrap()
                .execution_target
                .0
                > 2.0
        );
    }

    #[test]
    fn decision_replay_explicit_order_observation_uses_stable_order_ref() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.steps.extend([
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(62),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.5,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(63),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.8,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(64),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.8,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(65),
                action: DecisionScenarioAction::Position(PositionObservation {
                    qty: 0.8,
                    unrealized_pnl: 0.0,
                }),
            },
        ]);

        let report = run_decision_replay(&input).unwrap();

        assert_eq!(report.steps[2].signed_filled_native_qty, 0.5);
        assert_eq!(report.steps[2].simulated_position_qty, 0.5);
        assert_eq!(report.steps[2].observed_position_qty, 0.0);
        assert!(report.steps[2].estimated_taker_fee > 0.0);
        assert_close(report.steps[3].signed_filled_native_qty, 0.3);
        assert_eq!(report.steps[3].simulated_position_qty, 0.8);
        assert_eq!(report.steps[3].observed_position_qty, 0.0);
        assert_eq!(report.steps[4].signed_filled_native_qty, 0.0);
        assert_eq!(report.steps[4].estimated_taker_fee, 0.0);
        assert_eq!(report.steps[4].simulated_position_qty, 0.8);
        assert_eq!(report.steps[5].simulated_position_qty, 0.8);
        assert_eq!(report.steps[5].observed_position_qty, 0.8);
        assert!(report.steps[5].committed_decision.is_some());
    }

    #[test]
    fn decision_replay_normalizes_filled_without_reported_quantity() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.steps.push(DecisionScenarioStep {
            observed_at: input.started_at + Duration::minutes(2),
            action: DecisionScenarioAction::Order(DecisionOrderObservation {
                order_ref: "order-1".to_string(),
                filled_qty: 0.0,
                status: OrderStatus::Filled,
            }),
        });

        let report = run_decision_replay(&input).unwrap();
        let submitted_quantity = report
            .steps
            .iter()
            .flat_map(|step| &step.effects)
            .find_map(|effect| match effect {
                DecisionReplayEffect::SubmitOrder {
                    order_ref,
                    quantity,
                    ..
                } if order_ref == "order-1" => Some(*quantity),
                _ => None,
            })
            .unwrap();

        assert_close(
            report.steps[2].signed_filled_native_qty.abs(),
            submitted_quantity,
        );
        assert!(report.steps[2].estimated_taker_fee > 0.0);
        assert!(report.steps[2].effects.iter().any(|effect| matches!(
            effect,
            DecisionReplayEffect::FillOrder {
                order_ref,
                quantity,
                ..
            } if order_ref == "order-1" && (*quantity - submitted_quantity).abs() < f64::EPSILON
        )));
    }

    #[test]
    fn decision_replay_does_not_revive_terminal_order_from_stale_working_update() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::ImmediateFullAtOrderPrice);
        input.steps.extend([
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::minutes(2),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-2".to_string(),
                    filled_qty: 0.5,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::minutes(3),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-2".to_string(),
                    filled_qty: 0.5,
                    status: OrderStatus::Canceled,
                }),
            },
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::minutes(4),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-2".to_string(),
                    filled_qty: 0.5,
                    status: OrderStatus::New,
                }),
            },
            market_step(input.started_at + Duration::minutes(5), 100.1),
        ]);

        let report = run_decision_replay(&input).unwrap();

        assert!(!report.steps[5].effects.iter().any(|effect| matches!(
            effect,
            DecisionReplayEffect::FillOrder { order_ref, .. } if order_ref == "order-2"
        )));
    }

    #[test]
    fn decision_replay_keeps_late_fill_fact_after_manager_prunes_terminal_order() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.steps.extend([
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(61),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.5,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(62),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.5,
                    status: OrderStatus::Canceled,
                }),
            },
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(63),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.0,
                    status: OrderStatus::Filled,
                }),
            },
        ]);

        let report = run_decision_replay(&input).unwrap();

        assert_close(report.steps[4].signed_filled_native_qty.abs(), 0.5);
        assert!(report.steps[4].estimated_taker_fee > 0.0);
        assert_eq!(report.steps[4].simulated_position_qty, 1.0);
        assert_eq!(report.steps[4].observed_position_qty, 0.0);
        assert!(report.steps[4].effects.iter().any(|effect| matches!(
            effect,
            DecisionReplayEffect::FillOrder { order_ref, .. } if order_ref == "order-1"
        )));
    }

    #[test]
    fn decision_replay_does_not_report_hidden_stale_decision_as_committed() {
        let started_at = at(2026, 7, 1, 9, 0);
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.steps = vec![
            market_step(started_at, 95.0),
            DecisionScenarioStep {
                observed_at: started_at + Duration::seconds(31),
                action: DecisionScenarioAction::Position(PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                }),
            },
        ];

        let report = run_decision_replay(&input).unwrap();

        assert!(report.steps[0].committed_decision.is_some());
        assert!(report.steps[1].committed_decision.is_none());
    }

    #[test]
    fn decision_replay_immediate_model_waits_until_passive_order_is_marketable() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::ImmediateFullAtOrderPrice);
        input
            .steps
            .push(market_step(input.started_at + Duration::minutes(2), 100.1));

        let report = run_decision_replay(&input).unwrap();

        assert!(report.steps[1].signed_filled_native_qty > 0.0);
        assert!(report.steps[1].simulated_position_qty > 0.0);
        assert!(report.steps[1].effects.iter().any(|effect| matches!(
            effect,
            DecisionReplayEffect::FillOrder {
                side: Side::Buy,
                trade_notional,
                risk_increase_native_qty,
                position_before_qty,
                position_after_qty,
                ..
            } if (*trade_notional - 92.6).abs() < 1e-12
                && (*risk_increase_native_qty - 1.0).abs() < 1e-12
                && position_before_qty.abs() < f64::EPSILON
                && (*position_after_qty - 1.0).abs() < f64::EPSILON
        )));
        assert!(report.steps[2].signed_filled_native_qty < 0.0);
        assert!(report.steps[2].effects.iter().any(|effect| matches!(
            effect,
            DecisionReplayEffect::FillOrder {
                order_ref,
                side: Side::Sell,
                risk_increase_native_qty,
                ..
            } if order_ref == "order-2" && risk_increase_native_qty.abs() < f64::EPSILON
        )));
        assert!(report.final_simulated_position_qty.abs() < f64::EPSILON);
    }

    #[test]
    fn decision_replay_processes_manager_cancel_before_retained_order_fill() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::ImmediateFullAtOrderPrice);
        input.steps.extend([
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(61),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-2".to_string(),
                    filled_qty: 0.1,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
            market_step(input.started_at + Duration::seconds(62), 100.1),
        ]);

        let report = run_decision_replay(&input).unwrap();

        assert!(report.steps[3].effects.iter().any(|effect| matches!(
            effect,
            DecisionReplayEffect::CancelOrder { order_ref } if order_ref == "order-2"
        )));
        assert!(!report.steps[3].effects.iter().any(|effect| matches!(
            effect,
            DecisionReplayEffect::FillOrder { order_ref, .. } if order_ref == "order-2"
        )));
    }

    #[test]
    fn decision_replay_rejects_time_travel_with_step_context() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.steps.swap(0, 1);

        let error = run_decision_replay(&input).unwrap_err().to_string();

        assert!(error.contains("step 1"));
        assert!(error.contains("must not move backwards"));
    }

    #[test]
    fn decision_replay_rejects_invalid_rules_even_when_orders_never_fill() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.exchange_rules.taker_fee_rate = f64::NAN;

        let error = run_decision_replay(&input).unwrap_err().to_string();

        assert!(error.contains("taker_fee_rate"));
    }

    #[test]
    fn decision_replay_rejects_unknown_order_ref_and_cumulative_fill_regression() {
        let mut unknown = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        unknown.steps.push(DecisionScenarioStep {
            observed_at: unknown.started_at + Duration::minutes(2),
            action: DecisionScenarioAction::Order(DecisionOrderObservation {
                order_ref: "order-404".to_string(),
                filled_qty: 0.0,
                status: OrderStatus::New,
            }),
        });
        let error = run_decision_replay(&unknown).unwrap_err().to_string();
        assert!(error.contains("order-404"));

        let mut backwards = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        backwards.steps.extend([
            DecisionScenarioStep {
                observed_at: backwards.started_at + Duration::minutes(2),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.5,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
            DecisionScenarioStep {
                observed_at: backwards.started_at + Duration::minutes(3),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.4,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
        ]);
        let error = run_decision_replay(&backwards).unwrap_err().to_string();
        assert!(error.contains("must not move backwards"));
    }

    #[test]
    fn decision_replay_restart_invalidates_previous_session_order_refs() {
        let mut input = replay_input(0.0, DecisionReplayFillModel::NeverFill);
        input.steps.extend([
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(61),
                action: DecisionScenarioAction::Restart,
            },
            DecisionScenarioStep {
                observed_at: input.started_at + Duration::seconds(62),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.0,
                    status: OrderStatus::New,
                }),
            },
        ]);

        let error = run_decision_replay(&input).unwrap_err().to_string();

        assert!(error.contains("step 3"));
        assert!(error.contains("unknown current-session order_ref `order-1`"));
    }

    fn replay_input(initial_qty: f64, fill_model: DecisionReplayFillModel) -> DecisionReplayInput {
        let started_at = at(2026, 7, 1, 9, 0);
        DecisionReplayInput {
            definition: definition(),
            exchange_rules: rules(),
            started_at,
            initial_position: PositionObservation {
                qty: initial_qty,
                unrealized_pnl: 0.0,
            },
            steps: vec![
                market_step(started_at, 95.0),
                market_step(started_at + Duration::minutes(1), 92.5),
            ],
            fill_model,
        }
    }

    fn definition() -> TrackDefinition {
        TrackDefinition::try_new(
            TrackId::new("decision-replay"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 100.0,
                min_rebalance_units: 1.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: BandProtectionPolicy::Freeze,
                risk_acquisition: Default::default(),
            },
            Some(1_600.0),
            LossLimits {
                daily_loss_limit: 1_000.0,
                total_loss_limit: 5_000.0,
            },
            None,
        )
        .unwrap()
    }

    fn rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: QuantityKind::BaseAsset,
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.001,
        }
    }

    fn market_step(observed_at: DateTime<Utc>, price: f64) -> DecisionScenarioStep {
        DecisionScenarioStep {
            observed_at,
            action: DecisionScenarioAction::Market(MarketObservation::ExecutionQuote {
                execution_quote: ExecutionQuote {
                    best_bid: price - 0.1,
                    best_ask: price + 0.1,
                },
            }),
        }
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {actual} to be close to {expected}"
        );
    }
}
