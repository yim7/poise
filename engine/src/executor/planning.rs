use chrono::{DateTime, Utc};
use poise_core::events::ReplacementGateReason;
use poise_core::types::{ExchangeRules, Exposure, Side};
use serde::{Deserialize, Serialize};

use crate::execution_plan::ExecutionAction;
use crate::execution_plan::{is_meetable_minimum, round_to_step};
use crate::ports::OrderRequest;
use crate::runtime::{
    ExecutionRound, ExecutionSlot, ExecutorDiagnostics, ExecutorState, SlotState, WorkingOrder,
};
use crate::track::{Instrument, TrackId};

use super::round_policy::{
    RoundDecision, evaluate_round_policy, round_policy_input_from_state,
    round_policy_input_from_state_with_lifecycle,
};
use super::rebalance_trigger::{ActiveLifecycle, RebalanceTriggerDecision};
use super::{ExecutionMode, INVENTORY_CORE_SLOT, recording, slots};

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

#[derive(Debug, Clone)]
pub(super) struct SubmitIntentEvaluation {
    pub trigger_decision: RebalanceTriggerDecision,
    pub submit_hint: Option<PendingSubmitHint>,
}

const BPS_DENOMINATOR: f64 = 10_000.0;
const PASSIVE_REPLACEMENT_SAFETY_BUFFER_BPS: f64 = 15.0;
const REBALANCE_REPLACEMENT_SAFETY_BUFFER_BPS: f64 = 5.0;
const CATCH_UP_REPLACEMENT_SAFETY_BUFFER_BPS: f64 = 0.0;
const PASSIVE_STALE_REPRICE_AFTER_MS: i64 = 180_000;
const REBALANCE_STALE_REPRICE_AFTER_MS: i64 = 60_000;
const CATCH_UP_STALE_REPRICE_AFTER_MS: i64 = 20_000;
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
    let active_lifecycle = ActiveLifecycle::from_executor_state(executor_state);
    let round_decision = evaluate_round_policy(round_policy_input_from_state_with_lifecycle(
        &submit_intent.current_exposure,
        &submit_intent.target_exposure,
        executor_state,
        None,
        submit_intent.min_rebalance_units,
        submit_intent.observed_at,
        Some(active_lifecycle),
    ));
    let desired_orders = plan_desired_orders(&submit_intent, executor_state, &round_decision);
    let (effects, slots, replacement_gate_reason) = diff_desired_orders(
        &submit_intent,
        executor_state,
        &desired_orders,
        &round_decision.mode,
        &round_decision.trigger_decision,
    );

    ExecutorPlan {
        state: ExecutorState {
            active_round: round_decision.active_round,
            diagnostics: ExecutorDiagnostics {
                mode: round_decision.mode,
                inventory_gap: round_decision.inventory_gap,
                gap_started_at: round_decision.gap_started_at,
                last_reprice_at: if effects.iter().any(|effect| {
                    matches!(
                        effect,
                        ExecutionAction::SubmitOrder { .. } | ExecutionAction::CancelOrder { .. }
                    )
                }) {
                    Some(submit_intent.observed_at)
                } else {
                    executor_state.and_then(|state| state.diagnostics.last_reprice_at)
                },
                last_execution_reason: round_decision.last_execution_reason,
                recovery_anomaly: None,
            },
            slots,
            recent_terminal_orders: executor_state
                .map(|state| state.recent_terminal_orders.clone())
                .unwrap_or_default(),
            stats: round_decision.stats,
        },
        desired_orders,
        effects,
        replacement_gate_reason,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn current_submit_hint(input: SubmitIntentInput<'_>) -> Option<PendingSubmitHint> {
    evaluate_submit_intent_with_active_lifecycle(input, ActiveLifecycle::none(), None).submit_hint
}

pub(super) fn evaluate_submit_intent_with_active_lifecycle(
    input: SubmitIntentInput<'_>,
    active_lifecycle: ActiveLifecycle<'_>,
    executor_state: Option<&ExecutorState>,
) -> SubmitIntentEvaluation {
    let round_decision = evaluate_round_policy(round_policy_input_from_state_with_lifecycle(
        &input.current_exposure,
        &input.target_exposure,
        executor_state,
        None,
        input.min_rebalance_units,
        input.observed_at,
        Some(active_lifecycle),
    ));
    let submit_hint = desired_inventory_order_for_submit_intent(&input, executor_state, &round_decision)
        .map(|desired_order| {
            let request = desired_order_to_request(&input, &desired_order);
            PendingSubmitHint {
                request,
                target_exposure: desired_order.target_exposure,
            }
        });

    SubmitIntentEvaluation {
        trigger_decision: round_decision.trigger_decision,
        submit_hint,
    }
}

pub fn refresh_state(
    previous_state: &ExecutorState,
    current_exposure: &Exposure,
    target_exposure: &Exposure,
    min_rebalance_units: f64,
    observed_at: DateTime<Utc>,
) -> ExecutorState {
    let round_decision = evaluate_round_policy(round_policy_input_from_state(
        current_exposure,
        target_exposure,
        Some(previous_state),
        min_rebalance_units,
        observed_at,
    ));

    ExecutorState {
        active_round: round_decision.active_round,
        diagnostics: ExecutorDiagnostics {
            mode: round_decision.mode,
            inventory_gap: round_decision.inventory_gap,
            gap_started_at: round_decision.gap_started_at,
            last_reprice_at: previous_state.diagnostics.last_reprice_at,
            last_execution_reason: round_decision.last_execution_reason,
            recovery_anomaly: previous_state.diagnostics.recovery_anomaly.clone(),
        },
        slots: previous_state.slots.clone(),
        recent_terminal_orders: previous_state.recent_terminal_orders.clone(),
        stats: round_decision.stats,
    }
}

#[cfg(test)]
pub(crate) fn round_policy_input_for_test<'a>(
    current_exposure: &'a Exposure,
    desired_exposure: &'a Exposure,
    executor_state: Option<&'a ExecutorState>,
    min_rebalance_units: f64,
    observed_at: DateTime<Utc>,
) -> super::round_policy::RoundPolicyInput<'a> {
    round_policy_input_from_state(
        current_exposure,
        desired_exposure,
        executor_state,
        min_rebalance_units,
        observed_at,
    )
}

#[cfg(test)]
pub(crate) fn round_decision_for_test(
    input: ExecutorInput<'_>,
) -> super::round_policy::RoundDecision {
    let active_lifecycle = ActiveLifecycle::from_executor_state(input.executor_state);
    evaluate_round_policy(round_policy_input_from_state_with_lifecycle(
        &input.submit_intent.current_exposure,
        &input.submit_intent.target_exposure,
        input.executor_state,
        None,
        input.submit_intent.min_rebalance_units,
        input.submit_intent.observed_at,
        Some(active_lifecycle),
    ))
}

fn plan_desired_orders(
    input: &SubmitIntentInput<'_>,
    executor_state: Option<&ExecutorState>,
    round_decision: &RoundDecision,
) -> Vec<DesiredOrder> {
    desired_inventory_order_for_submit_intent(input, executor_state, round_decision)
        .into_iter()
        .collect()
}

fn desired_inventory_order_for_submit_intent(
    input: &SubmitIntentInput<'_>,
    executor_state: Option<&ExecutorState>,
    round_decision: &RoundDecision,
) -> Option<DesiredOrder> {
    match round_decision.trigger_decision {
        RebalanceTriggerDecision::TriggerFreshAction => {
            desired_inventory_order_for_target(input, &input.target_exposure)
        }
        RebalanceTriggerDecision::PreserveActiveLifecycle => desired_inventory_order_for_preserved_round(
            input,
            executor_state,
            &round_decision.mode,
            round_decision.active_round.as_ref(),
        ),
        RebalanceTriggerDecision::Suppress => None,
    }
}

fn desired_inventory_order_for_target(
    input: &SubmitIntentInput<'_>,
    target_exposure: &Exposure,
) -> Option<DesiredOrder> {
    let inventory_gap = input.current_exposure.delta(target_exposure);
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
        target_exposure: target_exposure.clone(),
        role: slots::role_for_target_change(&input.current_exposure, target_exposure),
    })
}

fn desired_inventory_order_for_preserved_round(
    input: &SubmitIntentInput<'_>,
    executor_state: Option<&ExecutorState>,
    mode: &ExecutionMode,
    active_round: Option<&ExecutionRound>,
) -> Option<DesiredOrder> {
    let state = executor_state?;
    let active_round = active_round.or(state.active_round.as_ref())?;
    let current_slot = state
        .slots
        .iter()
        .find(|slot| slot.slot == OrderSlot::new(INVENTORY_CORE_SLOT))
        ?;
    let current_order = current_slot.working_order.as_ref()?;
    let desired_order = desired_inventory_order_for_target(input, &active_round.target_exposure)?;

    match current_slot.state {
        SlotState::SubmitPending => {
            if !pending_order_should_be_replaced(
                mode,
                current_order,
                &desired_order,
                input.reference_price,
                input.exchange_rules,
            )
            {
                return None;
            }
            Some(desired_order)
        }
        SlotState::Working => {
            if desired_matches_working_order(&desired_order, current_order, input.exchange_rules) {
                return None;
            }
            let last_reprice_at = state.diagnostics.last_reprice_at?;
            let age_ms = (input.observed_at - last_reprice_at)
                .num_milliseconds()
                .max(0);
            if age_ms < stale_reprice_after_ms(mode) {
                return None;
            }
            current_order.order_id.as_ref()?;
            Some(desired_order)
        }
        SlotState::Empty => None,
    }
}

fn diff_desired_orders(
    input: &SubmitIntentInput<'_>,
    executor_state: Option<&ExecutorState>,
    desired_orders: &[DesiredOrder],
    mode: &ExecutionMode,
    trigger_decision: &RebalanceTriggerDecision,
) -> (
    Vec<ExecutionAction>,
    Vec<ExecutionSlot>,
    Option<ReplacementGateReason>,
) {
    let (current_slot, sibling_slots) = slots::split_inventory_core_slot(executor_state);
    let desired_order = desired_orders.first();

    match desired_order {
        None => {
            if matches!(
                trigger_decision,
                RebalanceTriggerDecision::PreserveActiveLifecycle
            ) && matches!(
                current_slot.state,
                SlotState::SubmitPending | SlotState::Working
            ) {
                return (
                    vec![ExecutionAction::NoOp],
                    slots::with_inventory_core_slot(sibling_slots, current_slot),
                    None,
                );
            }
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
                    mode,
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
    mode: &ExecutionMode,
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
    rules: &ExchangeRules,
) -> Option<ReplacementGateReason> {
    current_order.order_id.as_ref()?;
    replacement_gate_reason_for_pending_order(
        mode,
        current_order,
        desired_order,
        reference_price,
        rules,
    )
}

fn replacement_gate_reason_for_pending_order(
    mode: &ExecutionMode,
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
    rules: &ExchangeRules,
) -> Option<ReplacementGateReason> {
    if current_order.side != desired_order.side {
        return None;
    }

    let Some(improvement_ratio) =
        replacement_improvement_ratio(current_order, desired_order, reference_price)
    else {
        return None;
    };
    let threshold_rate = (rules.maker_fee_rate + rules.taker_fee_rate)
        + (replacement_safety_buffer_bps(mode) / BPS_DENOMINATOR);
    (improvement_ratio < threshold_rate).then(|| ReplacementGateReason::ImprovementBelowThreshold {
        improvement_bps: ratio_to_bps(improvement_ratio),
        threshold_bps: ratio_to_bps(threshold_rate),
    })
}

fn pending_order_should_be_replaced(
    mode: &ExecutionMode,
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
    rules: &ExchangeRules,
) -> bool {
    if matches!(mode, ExecutionMode::Passive) {
        return false;
    }

    if desired_matches_working_order(desired_order, current_order, rules) {
        return false;
    }

    replacement_gate_reason_for_pending_order(
        mode,
        current_order,
        desired_order,
        reference_price,
        rules,
    )
    .is_none()
}

fn replacement_improvement_ratio(
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
) -> Option<f64> {
    let price_improvement = match desired_order.side {
        Side::Buy => current_order.price - desired_order.price,
        Side::Sell => desired_order.price - current_order.price,
    };
    if price_improvement <= 0.0 || reference_price <= f64::EPSILON {
        return None;
    }
    Some(price_improvement / reference_price)
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

fn replacement_safety_buffer_bps(mode: &ExecutionMode) -> f64 {
    match mode {
        ExecutionMode::Passive => PASSIVE_REPLACEMENT_SAFETY_BUFFER_BPS,
        ExecutionMode::Rebalance => REBALANCE_REPLACEMENT_SAFETY_BUFFER_BPS,
        ExecutionMode::CatchUp => CATCH_UP_REPLACEMENT_SAFETY_BUFFER_BPS,
    }
}

fn stale_reprice_after_ms(mode: &ExecutionMode) -> i64 {
    match mode {
        ExecutionMode::Passive => PASSIVE_STALE_REPRICE_AFTER_MS,
        ExecutionMode::Rebalance => REBALANCE_STALE_REPRICE_AFTER_MS,
        ExecutionMode::CatchUp => CATCH_UP_STALE_REPRICE_AFTER_MS,
    }
}
