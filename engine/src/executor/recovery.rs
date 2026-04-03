use chrono::{DateTime, Utc};
use poise_core::types::{ExchangeRules, Exposure};

use crate::observation::OrderObservation;
use crate::ports::OrderRequest;
use crate::runtime::{ExecutorState, SlotState};
use crate::transition::TrackEffect;

use super::planning::evaluate_submit_intent_with_active_lifecycle;
use super::round_policy::{RoundDecision, evaluate_round_policy, round_policy_input_from_state};
use super::rebalance_trigger::{ActiveLifecycle, RebalanceTriggerDecision};
use super::{PendingSubmitHint, SubmitIntentInput, recording, slots};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAnomaly {
    UnknownLiveOrder,
    DuplicateLiveOrders,
    AmbiguousLiveOrder,
}

pub struct RecoveryInput<'a> {
    pub current_exposure: &'a Exposure,
    pub target_exposure: Option<&'a Exposure>,
    pub min_rebalance_units: f64,
    pub previous_state: Option<&'a ExecutorState>,
    pub live_orders: &'a [OrderObservation],
    pub pending_submit_hints: &'a [PendingSubmitHint],
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryResolution {
    Rebuilt {
        state: ExecutorState,
    },
    Anomaly {
        state: ExecutorState,
        anomaly: RecoveryAnomaly,
    },
}

pub struct SubmitRecoveryInput<'a> {
    pub exchange_rules: &'a ExchangeRules,
    pub previous_state: &'a ExecutorState,
    pub request: &'a OrderRequest,
    pub target_exposure: &'a Exposure,
    pub current_exposure: &'a Exposure,
    pub live_order: Option<&'a OrderObservation>,
    pub current_plan: Option<SubmitIntentInput<'a>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubmitRecoveryResolution {
    Proceed {
        state: ExecutorState,
        target_exposure: Exposure,
    },
    AwaitExchangeState,
    Recovered {
        state: ExecutorState,
    },
    Superseded {
        state: ExecutorState,
    },
}

impl SubmitRecoveryResolution {
    pub fn state(&self) -> Option<&ExecutorState> {
        match self {
            Self::Proceed { state, .. }
            | Self::Recovered { state }
            | Self::Superseded { state } => Some(state),
            Self::AwaitExchangeState => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitRecoveryPlan {
    pub resolution: SubmitRecoveryResolution,
    pub effects: Vec<TrackEffect>,
}

pub fn recover_submit_effect(input: SubmitRecoveryInput<'_>) -> SubmitRecoveryPlan {
    if input.previous_state.recovery_anomaly.is_some() {
        return SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::AwaitExchangeState,
            effects: vec![],
        };
    }

    let receipt_backed_order_id = input
        .previous_state
        .slots
        .iter()
        .find(|slot| slots::slot_matches_order(slot, &input.request.client_order_id, None))
        .and_then(|slot| slot.working_order.as_ref())
        .and_then(|order| order.order_id.clone());

    if let Some(receipt_backed_order_id) = receipt_backed_order_id {
        if let Some(live_order) = input.live_order {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Recovered {
                    state: recording::apply_order_observation(input.previous_state, live_order),
                },
                effects: vec![],
            };
        }

        if recording::target_exposure_reached(input.current_exposure, input.target_exposure) {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Recovered {
                    state: recording::clear_working_order_by_order_id(
                        input.previous_state,
                        &receipt_backed_order_id,
                    ),
                },
                effects: vec![],
            };
        }

        return SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::AwaitExchangeState,
            effects: vec![],
        };
    }

    let foreign_receipt_backed_order_exists = input.previous_state.slots.iter().any(|slot| {
        slot.working_order.as_ref().is_some_and(|order| {
            order.order_id.is_some() && order.client_order_id != input.request.client_order_id
        })
    });
    if foreign_receipt_backed_order_exists {
        return SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::AwaitExchangeState,
            effects: vec![],
        };
    }

    let active_lifecycle = ActiveLifecycle::from_executor_state(Some(input.previous_state));

    let current_plan_evaluation = input.current_plan.as_ref().map(|current_plan| {
        evaluate_submit_intent_with_active_lifecycle(current_plan.clone(), active_lifecycle)
    });
    let current_plan_submit = current_plan_evaluation
        .as_ref()
        .and_then(|evaluation| evaluation.submit_hint.as_ref());

    let matching_pending_submit_can_proceed = active_lifecycle
        .pending_submit_for_request(&input.request.client_order_id)
        .is_some_and(|slot| {
            current_plan_evaluation.as_ref().is_some_and(|evaluation| {
                evaluation.trigger_decision == RebalanceTriggerDecision::PreserveActiveLifecycle
            }) && pending_submit_matches_request(slot, input.request, input.exchange_rules)
        });

    if matching_pending_submit_can_proceed {
        let target_exposure = active_lifecycle
            .pending_submit_for_request(&input.request.client_order_id)
            .and_then(|slot| slot.working_order.as_ref())
            .map(|order| order.target_exposure.clone())
            .unwrap_or_else(|| input.target_exposure.clone());
        return SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Proceed {
                state: input.previous_state.clone(),
                target_exposure,
            },
            effects: vec![],
        };
    }

    if !submit_recovery_matches_current_plan(
        input.request,
        current_plan_submit,
        input.exchange_rules,
    ) {
        let cleared_state =
            recording::clear_pending_submit(input.previous_state, &input.request.client_order_id);
        if let Some(next_submit) = current_plan_submit {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Superseded {
                    state: recording::record_submit_request(
                        &cleared_state,
                        &next_submit.request,
                        next_submit.target_exposure.clone(),
                    ),
                },
                effects: vec![TrackEffect::SubmitOrder {
                    request: next_submit.request.clone(),
                    target_exposure: next_submit.target_exposure.clone(),
                }],
            };
        }
        return SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded {
                state: cleared_state,
            },
            effects: vec![],
        };
    }

    let next_target_exposure = input
        .current_plan
        .as_ref()
        .and(current_plan_submit)
        .map(|submit| submit.target_exposure.clone())
        .unwrap_or_else(|| input.target_exposure.clone());

    SubmitRecoveryPlan {
        resolution: SubmitRecoveryResolution::Proceed {
            state: recording::record_submit_request(
                input.previous_state,
                input.request,
                next_target_exposure.clone(),
            ),
            target_exposure: next_target_exposure,
        },
        effects: vec![],
    }
}

pub fn recover_working_orders(input: RecoveryInput<'_>) -> RecoveryResolution {
    let desired_exposure = input.target_exposure.unwrap_or(input.current_exposure);
    let round_decision = evaluate_round_policy(round_policy_input_from_state(
        input.current_exposure,
        desired_exposure,
        input.previous_state,
        input.min_rebalance_units,
        input.observed_at,
    ));
    let base_state = input
        .previous_state
        .cloned()
        .unwrap_or_else(|| ExecutorState {
            mode: round_decision.mode.clone(),
            inventory_gap: round_decision.inventory_gap.clone(),
            gap_started_at: round_decision.gap_started_at,
            last_reprice_at: None,
            slots: vec![slots::empty_inventory_core_slot()],
            recent_terminal_orders: Vec::new(),
            last_execution_reason: round_decision.last_execution_reason.clone(),
            recovery_anomaly: None,
            stats: round_decision.stats.clone(),
        });
    let base_state = apply_round_decision(base_state, &round_decision);

    if input.live_orders.is_empty() {
        let has_pending_receipt_backed_slot = base_state.slots.iter().any(|slot| {
            slot.working_order.as_ref().is_some_and(|order| {
                order.order_id.is_some()
                    && input
                        .pending_submit_hints
                        .iter()
                        .any(|hint| hint.request.client_order_id == order.client_order_id)
            })
        });
        if has_pending_receipt_backed_slot {
            return recovery_anomaly(&base_state, RecoveryAnomaly::UnknownLiveOrder);
        }

        let mut state = base_state;
        state.slots = vec![slots::empty_inventory_core_slot()];
        state.recovery_anomaly = None;
        return RecoveryResolution::Rebuilt { state };
    }

    let mut claimed_orders = vec![None; base_state.slots.len()];
    for live_order in input.live_orders {
        let matching_indexes = base_state
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slots::slot_matches_order(
                    slot,
                    &live_order.client_order_id,
                    Some(live_order.order_id.as_str()),
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();

        let matched_index = match matching_indexes.as_slice() {
            [] => return recovery_anomaly(&base_state, RecoveryAnomaly::UnknownLiveOrder),
            [index] => *index,
            _ => return recovery_anomaly(&base_state, RecoveryAnomaly::AmbiguousLiveOrder),
        };

        let slot = &base_state.slots[matched_index];
        let expected_side = slot.working_order.as_ref().map(|order| order.side);
        if expected_side.is_some() && expected_side != Some(live_order.side) {
            return recovery_anomaly(&base_state, RecoveryAnomaly::AmbiguousLiveOrder);
        }

        if claimed_orders[matched_index].is_some() {
            return recovery_anomaly(&base_state, RecoveryAnomaly::DuplicateLiveOrders);
        }
        claimed_orders[matched_index] = Some(live_order);
    }

    let rebuilt_slots = base_state
        .slots
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            claimed_orders[index].map(|live_order| {
                slots::rebuild_slot_from_live_order(
                    slot,
                    live_order,
                    input.target_exposure,
                    input.current_exposure,
                )
            })
        })
        .collect();

    let mut state = base_state;
    state.slots = rebuilt_slots;
    state.recovery_anomaly = None;
    RecoveryResolution::Rebuilt { state }
}

fn apply_round_decision(mut state: ExecutorState, round_decision: &RoundDecision) -> ExecutorState {
    state.mode = round_decision.mode.clone();
    state.inventory_gap = round_decision.inventory_gap.clone();
    state.gap_started_at = round_decision.gap_started_at;
    state.last_execution_reason = round_decision.last_execution_reason.clone();
    state.stats = round_decision.stats.clone();
    state
}

pub fn submit_requests_match(
    left: &OrderRequest,
    right: &OrderRequest,
    exchange_rules: &ExchangeRules,
) -> bool {
    left.instrument == right.instrument
        && left.side == right.side
        && left.reduce_only == right.reduce_only
        && values_match_with_step(left.price, right.price, exchange_rules.price_tick)
        && values_match_with_step(left.quantity, right.quantity, exchange_rules.quantity_step)
}

fn recovery_anomaly(base_state: &ExecutorState, anomaly: RecoveryAnomaly) -> RecoveryResolution {
    let mut state = base_state.clone();
    state.slots = vec![slots::empty_inventory_core_slot()];
    state.recovery_anomaly = Some(anomaly.clone());
    RecoveryResolution::Anomaly { state, anomaly }
}

fn submit_recovery_matches_current_plan(
    request: &OrderRequest,
    current_plan_submit: Option<&PendingSubmitHint>,
    exchange_rules: &ExchangeRules,
) -> bool {
    current_plan_submit
        .map(|submit| submit_requests_match(request, &submit.request, exchange_rules))
        .unwrap_or(false)
}

fn pending_submit_matches_request(
    slot: &crate::runtime::ExecutionSlot,
    request: &OrderRequest,
    exchange_rules: &ExchangeRules,
) -> bool {
    let Some(order) = slot.working_order.as_ref() else {
        return false;
    };
    slot.state == SlotState::SubmitPending
        && order.client_order_id == request.client_order_id
        && order.side == request.side
        && slots::role_for_reduce_only(request.reduce_only) == order.role
        && values_match_with_step(order.price, request.price, exchange_rules.price_tick)
        && values_match_with_step(
            order.quantity,
            request.quantity,
            exchange_rules.quantity_step,
        )
}

fn values_match_with_step(left: f64, right: f64, step: f64) -> bool {
    let tolerance = if step <= f64::EPSILON {
        f64::EPSILON * 16.0
    } else {
        step / 1_000_000.0
    };
    (left - right).abs() <= tolerance
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
    current_exposure: &Exposure,
    desired_exposure: &Exposure,
    executor_state: Option<&ExecutorState>,
    min_rebalance_units: f64,
    observed_at: DateTime<Utc>,
) -> RoundDecision {
    evaluate_round_policy(round_policy_input_from_state(
        current_exposure,
        desired_exposure,
        executor_state,
        min_rebalance_units,
        observed_at,
    ))
}
