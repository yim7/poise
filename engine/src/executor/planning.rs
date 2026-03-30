use chrono::{DateTime, Utc};
use grid_core::events::ReplacementGateReason;
use grid_core::types::{ExchangeRules, Exposure, Side};
use serde::{Deserialize, Serialize};

use crate::execution_plan::ExecutionAction;
use crate::execution_plan::{is_meetable_minimum, round_to_step};
use crate::grid::{GridId, Instrument};
use crate::ports::OrderRequest;
use crate::runtime::{ExecutionSlot, ExecutionStats, ExecutorState, SlotState, WorkingOrder};

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
    pub grid_id: &'a GridId,
    pub instrument: &'a Instrument,
    pub exchange_rules: &'a ExchangeRules,
    pub base_qty_per_unit: f64,
    pub current_exposure: Exposure,
    pub target_exposure: Exposure,
    pub reference_price: f64,
    pub executor_state: Option<&'a ExecutorState>,
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
const BINANCE_TAKER_FEE_RATE: f64 = 0.0004;
const BPS_DENOMINATOR: f64 = 10_000.0;

pub fn plan(input: ExecutorInput<'_>) -> ExecutorPlan {
    let inventory_gap = input.current_exposure.delta(&input.target_exposure);
    let gap_started_at =
        resolve_gap_started_at(input.executor_state, &inventory_gap, input.observed_at);
    let gap_age_ms = gap_started_at
        .map(|started_at| (input.observed_at - started_at).num_milliseconds().max(0))
        .unwrap_or(0);
    let mode = resolve_mode(&inventory_gap, gap_age_ms);
    let last_execution_reason = resolve_reason(input.executor_state, &mode);
    let stats = update_stats(
        input.executor_state,
        input.observed_at,
        &inventory_gap,
        gap_age_ms,
    );
    let desired_orders = plan_desired_orders(&input, &inventory_gap);
    let (effects, slots, replacement_gate_reason) = diff_desired_orders(&input, &desired_orders);

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
                Some(input.observed_at)
            } else {
                input.executor_state.and_then(|state| state.last_reprice_at)
            },
            slots,
            last_execution_reason,
            recovery_anomaly: None,
            stats,
        },
        desired_orders,
        effects,
        replacement_gate_reason,
    }
}

pub fn current_submit_hint(input: ExecutorInput<'_>) -> Option<PendingSubmitHint> {
    let plan = plan(input);
    match plan.effects.as_slice() {
        [
            ExecutionAction::SubmitOrder {
                request,
                target_exposure,
            },
        ] => Some(PendingSubmitHint {
            request: request.clone(),
            target_exposure: target_exposure.clone(),
        }),
        _ => None,
    }
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

fn plan_desired_orders(input: &ExecutorInput<'_>, _inventory_gap: &Exposure) -> Vec<DesiredOrder> {
    desired_inventory_order(
        input.exchange_rules,
        input.base_qty_per_unit,
        &input.current_exposure,
        &input.target_exposure,
        input.reference_price,
    )
    .into_iter()
    .collect()
}

fn desired_inventory_order(
    exchange_rules: &ExchangeRules,
    base_qty_per_unit: f64,
    current_exposure: &Exposure,
    target_exposure: &Exposure,
    reference_price: f64,
) -> Option<DesiredOrder> {
    let inventory_gap = current_exposure.delta(target_exposure);
    let side = Side::from_exposure(&inventory_gap)?;
    let price = round_to_step(reference_price, exchange_rules.price_tick);
    let quantity = round_to_step(
        inventory_gap.0.abs() * base_qty_per_unit,
        exchange_rules.quantity_step,
    );
    if !is_meetable_minimum(price, quantity, exchange_rules) {
        return None;
    }

    Some(DesiredOrder {
        slot: OrderSlot::new(INVENTORY_CORE_SLOT),
        side,
        price,
        quantity,
        target_exposure: target_exposure.clone(),
        role: match side {
            Side::Buy => OrderRole::IncreaseInventory,
            Side::Sell => OrderRole::DecreaseInventory,
        },
    })
}

fn diff_desired_orders(
    input: &ExecutorInput<'_>,
    desired_orders: &[DesiredOrder],
) -> (
    Vec<ExecutionAction>,
    Vec<ExecutionSlot>,
    Option<ReplacementGateReason>,
) {
    let (current_slot, sibling_slots) = slots::split_inventory_core_slot(input.executor_state);
    let desired_order = desired_orders.first();

    match desired_order {
        None => {
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
    input: &ExecutorInput<'_>,
    desired_order: &DesiredOrder,
) -> OrderRequest {
    OrderRequest {
        instrument: input.instrument.clone(),
        side: desired_order.side,
        price: desired_order.price,
        quantity: desired_order.quantity,
        client_order_id: format!(
            "{}-{}",
            input.grid_id.as_str(),
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
    let threshold_rate =
        (BINANCE_TAKER_FEE_RATE * 2.0) + (REPLACEMENT_SAFETY_BUFFER_BPS / BPS_DENOMINATOR);
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
