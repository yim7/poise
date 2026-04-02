use chrono::{DateTime, Utc};
use poise_core::events::ReplacementGateReason;
use poise_core::types::{ExchangeRules, Exposure, Side};
use serde::{Deserialize, Serialize};

use crate::execution_plan::ExecutionAction;
use crate::execution_plan::{is_meetable_minimum, round_to_step};
use crate::ports::OrderRequest;
use crate::runtime::{ExecutionSlot, ExecutionStats, ExecutorState, SlotState, WorkingOrder};
use crate::track::{Instrument, TrackId};

use super::{ExecutionMode, ExecutionReason, INVENTORY_CORE_SLOT, recording, slots};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderRole {
    IncreaseInventory,
    DecreaseInventory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderSlot(pub String);

impl OrderSlot {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesiredOrder {
    pub slot: OrderSlot,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub target_exposure: Exposure,
    pub role: OrderRole,
}

pub struct ExecutorInput<'a> {
    pub submit_intent: SubmitIntentInput<'a>,
    pub executor_state: Option<&'a ExecutorState>,
}

#[derive(Debug, Clone)]
pub struct SubmitIntentInput<'a> {
    pub track_id: &'a TrackId,
    pub instrument: &'a Instrument,
    pub exchange_rules: &'a ExchangeRules,
    pub base_qty_per_unit: f64,
    pub min_rebalance_units: f64,
    pub current_exposure: Exposure,
    pub target_exposure: Exposure,
    pub reference_price: f64,
    pub observed_at: DateTime<Utc>,
}

pub struct ExecutorPlan {
    pub state: ExecutorState,
    #[cfg_attr(not(test), allow(dead_code))]
    pub desired_orders: Vec<DesiredOrder>,
    pub effects: Vec<ExecutionAction>,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSubmitHint {
    pub request: OrderRequest,
    pub target_exposure: Exposure,
}

const REBALANCE_GAP_THRESHOLD: f64 = 2.0;
const CATCH_UP_GAP_THRESHOLD: f64 = 5.0;
const REBALANCE_AGE_MS: i64 = 60_000;
const CATCH_UP_AGE_MS: i64 = 180_000;
const REPLACEMENT_SAFETY_BUFFER_BPS: f64 = 5.0;
const BPS_DENOMINATOR: f64 = 10_000.0;
const MIN_REBALANCE_COMPARISON_TOLERANCE_FACTOR: f64 = 16.0;

impl<'a> ExecutorInput<'a> {
    pub fn new(
        submit_intent: SubmitIntentInput<'a>,
        executor_state: Option<&'a ExecutorState>,
    ) -> Self {
        Self {
            submit_intent,
            executor_state,
        }
    }
}

pub fn plan(input: ExecutorInput<'_>) -> ExecutorPlan {
    let ExecutorInput {
        submit_intent,
        executor_state,
    } = input;
    let inventory_gap = submit_intent
        .current_exposure
        .delta(&submit_intent.target_exposure);
    let gap_started_at =
        resolve_gap_started_at(executor_state, &inventory_gap, submit_intent.observed_at);
    let gap_age_ms = gap_started_at
        .map(|started_at| {
            (submit_intent.observed_at - started_at)
                .num_milliseconds()
                .max(0)
        })
        .unwrap_or(0);
    let mode = resolve_mode(&inventory_gap, gap_age_ms);
    let last_execution_reason = resolve_reason(executor_state, &mode);
    let stats = update_stats(
        executor_state,
        submit_intent.observed_at,
        &inventory_gap,
        gap_age_ms,
    );
    let desired_orders = plan_desired_orders(&submit_intent);
    let (effects, slots, replacement_gate_reason) =
        diff_desired_orders(&submit_intent, executor_state, &desired_orders);

    ExecutorPlan {
        state: ExecutorState {
            mode,
            inventory_gap,
            gap_started_at,
            last_reprice_at: if effects.iter().any(|effect| {
                matches!(
                    effect,
                    ExecutionAction::SubmitOrder { .. } | ExecutionAction::CancelOrder { .. }
                )
            }) {
                Some(submit_intent.observed_at)
            } else {
                executor_state.and_then(|state| state.last_reprice_at)
            },
            slots,
            recent_terminal_orders: executor_state
                .map(|state| state.recent_terminal_orders.clone())
                .unwrap_or_default(),
            last_execution_reason,
            recovery_anomaly: None,
            stats,
        },
        desired_orders,
        effects,
        replacement_gate_reason,
    }
}

pub fn current_submit_hint(input: SubmitIntentInput<'_>) -> Option<PendingSubmitHint> {
    let desired_order = desired_inventory_order_for_submit_intent(&input)?;
    let request = desired_order_to_request(&input, &desired_order);
    Some(PendingSubmitHint {
        request,
        target_exposure: desired_order.target_exposure,
    })
}

pub fn refresh_state(
    previous_state: &ExecutorState,
    current_exposure: &Exposure,
    target_exposure: &Exposure,
    observed_at: DateTime<Utc>,
) -> ExecutorState {
    let inventory_gap = current_exposure.delta(target_exposure);
    let gap_started_at = resolve_gap_started_at(Some(previous_state), &inventory_gap, observed_at);
    let gap_age_ms = gap_started_at
        .map(|started_at| (observed_at - started_at).num_milliseconds().max(0))
        .unwrap_or(0);
    let mode = resolve_mode(&inventory_gap, gap_age_ms);
    let last_execution_reason = resolve_reason(Some(previous_state), &mode);
    let stats = update_stats(
        Some(previous_state),
        observed_at,
        &inventory_gap,
        gap_age_ms,
    );

    ExecutorState {
        mode,
        inventory_gap,
        gap_started_at,
        last_reprice_at: previous_state.last_reprice_at,
        slots: previous_state.slots.clone(),
        recent_terminal_orders: previous_state.recent_terminal_orders.clone(),
        last_execution_reason,
        recovery_anomaly: previous_state.recovery_anomaly.clone(),
        stats,
    }
}

fn resolve_gap_started_at(
    previous_state: Option<&ExecutorState>,
    inventory_gap: &Exposure,
    observed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if inventory_gap.is_zero() {
        return None;
    }

    previous_state
        .and_then(|state| {
            (!state.inventory_gap.is_zero()
                && state.inventory_gap.0.signum() == inventory_gap.0.signum())
            .then_some(state.gap_started_at)
            .flatten()
        })
        .or(Some(observed_at))
}

fn resolve_mode(inventory_gap: &Exposure, gap_age_ms: i64) -> ExecutionMode {
    let abs_gap = inventory_gap.0.abs();
    if abs_gap >= CATCH_UP_GAP_THRESHOLD || gap_age_ms >= CATCH_UP_AGE_MS {
        return ExecutionMode::CatchUp;
    }
    if abs_gap >= REBALANCE_GAP_THRESHOLD || gap_age_ms >= REBALANCE_AGE_MS {
        return ExecutionMode::Rebalance;
    }
    ExecutionMode::Passive
}

fn resolve_reason(
    previous_state: Option<&ExecutorState>,
    mode: &ExecutionMode,
) -> Option<ExecutionReason> {
    let previous_mode = previous_state.map(|state| &state.mode);
    if previous_mode == Some(mode) {
        return previous_state.and_then(|state| state.last_execution_reason.clone());
    }

    Some(match mode {
        ExecutionMode::Passive => ExecutionReason::GapEnteredPassive,
        ExecutionMode::Rebalance => ExecutionReason::GapEscalatedToRebalance,
        ExecutionMode::CatchUp => ExecutionReason::GapEscalatedToCatchUp,
    })
}

fn update_stats(
    previous_state: Option<&ExecutorState>,
    observed_at: DateTime<Utc>,
    inventory_gap: &Exposure,
    gap_age_ms: i64,
) -> ExecutionStats {
    let started_at = previous_state
        .map(|state| state.stats.started_at)
        .unwrap_or(observed_at);
    let previous_max_gap = previous_state
        .map(|state| state.stats.max_inventory_gap_abs.0.abs())
        .unwrap_or(0.0);
    let previous_max_age = previous_state
        .map(|state| state.stats.max_gap_age_ms)
        .unwrap_or(0);

    ExecutionStats {
        started_at,
        max_inventory_gap_abs: Exposure(previous_max_gap.max(inventory_gap.0.abs())),
        max_gap_age_ms: previous_max_age.max(gap_age_ms),
    }
}

fn plan_desired_orders(input: &SubmitIntentInput<'_>) -> Vec<DesiredOrder> {
    desired_inventory_order_for_submit_intent(input)
        .into_iter()
        .collect()
}

fn desired_inventory_order_for_submit_intent(
    input: &SubmitIntentInput<'_>,
) -> Option<DesiredOrder> {
    let inventory_gap = input.current_exposure.delta(&input.target_exposure);
    if inventory_gap_below_min_rebalance_units(&inventory_gap, input.min_rebalance_units) {
        return None;
    }
    let side = Side::from_exposure(&inventory_gap)?;
    let price = round_to_step(input.reference_price, input.exchange_rules.price_tick);
    let quantity = round_to_step(
        inventory_gap.0.abs() * input.base_qty_per_unit,
        input.exchange_rules.quantity_step,
    );
    if quantity <= f64::EPSILON {
        return None;
    }
    if !is_meetable_minimum(price, quantity, input.exchange_rules) {
        return None;
    }

    Some(DesiredOrder {
        slot: OrderSlot::new(INVENTORY_CORE_SLOT),
        side,
        price,
        quantity,
        target_exposure: input.target_exposure.clone(),
        role: slots::role_for_target_change(&input.current_exposure, &input.target_exposure),
    })
}

fn inventory_gap_below_min_rebalance_units(
    inventory_gap: &Exposure,
    min_rebalance_units: f64,
) -> bool {
    if min_rebalance_units <= f64::EPSILON {
        return false;
    }

    let abs_gap = inventory_gap.0.abs();
    let tolerance = f64::EPSILON
        * MIN_REBALANCE_COMPARISON_TOLERANCE_FACTOR
        * abs_gap.max(min_rebalance_units).max(1.0);
    abs_gap + tolerance < min_rebalance_units
}

fn diff_desired_orders(
    input: &SubmitIntentInput<'_>,
    executor_state: Option<&ExecutorState>,
    desired_orders: &[DesiredOrder],
) -> (
    Vec<ExecutionAction>,
    Vec<ExecutionSlot>,
    Option<ReplacementGateReason>,
) {
    let (current_slot, sibling_slots) = slots::split_inventory_core_slot(executor_state);
    let desired_order = desired_orders.first();

    match desired_order {
        None => {
            if current_slot.state == SlotState::SubmitPending {
                return (
                    vec![ExecutionAction::NoOp],
                    slots::with_inventory_core_slot(sibling_slots, current_slot),
                    None,
                );
            }
            if let Some(order_id) = current_slot
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.clone())
            {
                return (
                    vec![ExecutionAction::CancelOrder {
                        instrument: input.instrument.clone(),
                        order_id,
                    }],
                    slots::with_inventory_core_slot(sibling_slots, current_slot),
                    None,
                );
            }
            (
                vec![ExecutionAction::NoOp],
                slots::with_inventory_core_slot(sibling_slots, slots::empty_inventory_core_slot()),
                None,
            )
        }
        Some(desired_order) if current_slot.state == SlotState::Empty => {
            let request = desired_order_to_request(input, desired_order);
            (
                vec![ExecutionAction::SubmitOrder {
                    request: request.clone(),
                    target_exposure: desired_order.target_exposure.clone(),
                }],
                slots::with_inventory_core_slot(
                    sibling_slots,
                    recording::submit_pending_slot(desired_order, &request),
                ),
                None,
            )
        }
        Some(desired_order) => {
            let current_order = current_slot.working_order.as_ref();
            if let Some(current_order) = current_order {
                if desired_matches_working_order(desired_order, current_order, input.exchange_rules)
                {
                    return (
                        vec![ExecutionAction::NoOp],
                        slots::with_inventory_core_slot(sibling_slots, current_slot),
                        Some(ReplacementGateReason::RoundedMatch),
                    );
                }

                if let Some(reason) = replacement_gate_reason_for_working_order(
                    current_order,
                    desired_order,
                    input.reference_price,
                    input.exchange_rules,
                ) {
                    return (
                        vec![ExecutionAction::NoOp],
                        slots::with_inventory_core_slot(sibling_slots, current_slot),
                        Some(reason),
                    );
                }

                if let Some(order_id) = current_order.order_id.clone() {
                    let request = desired_order_to_request(input, desired_order);
                    return (
                        vec![
                            ExecutionAction::CancelOrder {
                                instrument: input.instrument.clone(),
                                order_id,
                            },
                            ExecutionAction::SubmitOrder {
                                request: request.clone(),
                                target_exposure: desired_order.target_exposure.clone(),
                            },
                        ],
                        slots::with_inventory_core_slot(sibling_slots, current_slot),
                        None,
                    );
                }
            }

            (
                vec![ExecutionAction::NoOp],
                slots::with_inventory_core_slot(sibling_slots, current_slot),
                None,
            )
        }
    }
}

fn desired_order_to_request(
    input: &SubmitIntentInput<'_>,
    desired_order: &DesiredOrder,
) -> OrderRequest {
    OrderRequest {
        instrument: input.instrument.clone(),
        side: desired_order.side,
        price: desired_order.price,
        quantity: desired_order.quantity,
        client_order_id: format!(
            "{}-{}",
            input.track_id.as_str(),
            input.observed_at.timestamp_millis()
        ),
        reduce_only: desired_order.role == OrderRole::DecreaseInventory,
    }
}

fn desired_matches_working_order(
    desired_order: &DesiredOrder,
    current_order: &WorkingOrder,
    rules: &ExchangeRules,
) -> bool {
    desired_order.side == current_order.side
        && desired_order.role == current_order.role
        && rounded_values_match(desired_order.price, current_order.price, rules.price_tick)
        && rounded_values_match(
            desired_order.quantity,
            current_order.quantity,
            rules.quantity_step,
        )
}

fn replacement_gate_reason_for_working_order(
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
    rules: &ExchangeRules,
) -> Option<ReplacementGateReason> {
    if current_order.order_id.is_none() {
        return None;
    }
    if current_order.side != desired_order.side {
        return None;
    }
    if !rounded_values_match(
        current_order.quantity,
        desired_order.quantity,
        rules.quantity_step,
    ) {
        return None;
    }

    let improvement_ratio =
        replacement_improvement_ratio(current_order, desired_order, reference_price);
    let threshold_rate = (rules.maker_fee_rate + rules.taker_fee_rate)
        + (REPLACEMENT_SAFETY_BUFFER_BPS / BPS_DENOMINATOR);
    (improvement_ratio < threshold_rate).then(|| ReplacementGateReason::ImprovementBelowThreshold {
        improvement_bps: ratio_to_bps(improvement_ratio),
        threshold_bps: ratio_to_bps(threshold_rate),
    })
}

fn replacement_improvement_ratio(
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
) -> f64 {
    let price_improvement = match desired_order.side {
        Side::Buy => current_order.price - desired_order.price,
        Side::Sell => desired_order.price - current_order.price,
    };
    if price_improvement <= 0.0 || reference_price <= f64::EPSILON {
        return 0.0;
    }
    price_improvement / reference_price
}

fn rounded_values_match(left: f64, right: f64, step: f64) -> bool {
    let tolerance = if step <= f64::EPSILON {
        f64::EPSILON * 16.0
    } else {
        (step * 1e-9).max(f64::EPSILON * 16.0)
    };
    (left - right).abs() <= tolerance
}

fn ratio_to_bps(ratio: f64) -> f64 {
    ((ratio * BPS_DENOMINATOR) * 10.0).round() / 10.0
}
