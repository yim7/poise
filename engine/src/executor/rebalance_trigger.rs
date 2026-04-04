use poise_core::types::{Exposure, Side};

use crate::runtime::{ExecutionSlot, ExecutorState, SlotState};

use super::{INVENTORY_CORE_SLOT, slots};

const MIN_REBALANCE_COMPARISON_TOLERANCE_FACTOR: f64 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ActiveLifecycle<'a> {
    slot: Option<&'a ExecutionSlot>,
}

impl<'a> ActiveLifecycle<'a> {
    pub(super) fn none() -> Self {
        Self { slot: None }
    }

    pub(super) fn from_executor_state(executor_state: Option<&'a ExecutorState>) -> Self {
        let slot = executor_state.and_then(|state| {
            state
                .slots
                .iter()
                .find(|slot| slot.slot.0 == INVENTORY_CORE_SLOT)
        });
        Self::from_slot(slot)
    }

    fn from_slot(slot: Option<&'a ExecutionSlot>) -> Self {
        Self {
            slot: slot.filter(|slot| {
                matches!(slot.state, SlotState::SubmitPending | SlotState::Working)
                    && slot.working_order.is_some()
            }),
        }
    }

    pub(super) fn slot(&self) -> Option<&'a ExecutionSlot> {
        self.slot
    }

    pub(super) fn pending_submit_for_request(
        &self,
        client_order_id: &str,
    ) -> Option<&'a ExecutionSlot> {
        self.slot.filter(|slot| {
            slot.state == SlotState::SubmitPending
                && slot
                    .working_order
                    .as_ref()
                    .is_some_and(|order| order.client_order_id == client_order_id)
        })
    }
}

pub(super) struct RebalanceTriggerInput<'a> {
    pub current_exposure: &'a Exposure,
    pub latest_desired_exposure: &'a Exposure,
    pub active_round_desired_exposure: Option<&'a Exposure>,
    pub min_rebalance_units: f64,
    pub active_lifecycle: ActiveLifecycle<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RebalanceTriggerDecision {
    PreserveActiveLifecycle,
    TriggerFreshAction,
    Suppress,
}

pub(super) fn evaluate_rebalance_trigger(
    input: RebalanceTriggerInput<'_>,
) -> RebalanceTriggerDecision {
    if active_lifecycle_should_be_preserved(
        &input.active_lifecycle,
        input.active_round_desired_exposure,
        input.current_exposure,
        input.latest_desired_exposure,
        input.min_rebalance_units,
    ) {
        return RebalanceTriggerDecision::PreserveActiveLifecycle;
    }

    let fresh_trigger_delta = input.current_exposure.delta(input.latest_desired_exposure);
    if !trigger_delta_below_min_rebalance_units(&fresh_trigger_delta, input.min_rebalance_units) {
        return RebalanceTriggerDecision::TriggerFreshAction;
    }

    RebalanceTriggerDecision::Suppress
}

fn active_lifecycle_should_be_preserved(
    active_lifecycle: &ActiveLifecycle<'_>,
    active_round_desired_exposure: Option<&Exposure>,
    current_exposure: &Exposure,
    latest_desired_exposure: &Exposure,
    min_rebalance_units: f64,
) -> bool {
    let Some(anchor) = active_round_desired_exposure else {
        return false;
    };

    let trigger_delta = anchor.delta(latest_desired_exposure);
    trigger_delta_below_min_rebalance_units(&trigger_delta, min_rebalance_units)
        && active_lifecycle_matches_latest_target(
            active_lifecycle.slot(),
            current_exposure,
            latest_desired_exposure,
        )
}

fn trigger_delta_below_min_rebalance_units(
    trigger_delta: &Exposure,
    min_rebalance_units: f64,
) -> bool {
    if min_rebalance_units <= f64::EPSILON {
        return false;
    }

    let abs_gap = trigger_delta.0.abs();
    let tolerance = f64::EPSILON
        * MIN_REBALANCE_COMPARISON_TOLERANCE_FACTOR
        * abs_gap.max(min_rebalance_units).max(1.0);
    abs_gap + tolerance < min_rebalance_units
}

fn active_lifecycle_matches_latest_target(
    active_slot: Option<&ExecutionSlot>,
    current_exposure: &Exposure,
    latest_desired_exposure: &Exposure,
) -> bool {
    let Some(slot) = active_slot else {
        return false;
    };
    if !matches!(slot.state, SlotState::SubmitPending | SlotState::Working) {
        return false;
    }
    let Some(order) = slot.working_order.as_ref() else {
        return false;
    };

    let inventory_gap = current_exposure.delta(latest_desired_exposure);
    let Some(expected_side) = Side::from_exposure(&inventory_gap) else {
        return false;
    };
    let expected_role = slots::role_for_target_change(current_exposure, latest_desired_exposure);

    if order.side != expected_side {
        return false;
    }

    if order.role == expected_role {
        return true;
    }

    reduce_only_order_still_converges(order, current_exposure, latest_desired_exposure)
}

fn reduce_only_order_still_converges(
    order: &crate::runtime::WorkingOrder,
    current_exposure: &Exposure,
    latest_desired_exposure: &Exposure,
) -> bool {
    order.role == super::OrderRole::DecreaseInventory
        && current_exposure.0.abs() > f64::EPSILON
        && latest_desired_exposure.0.abs() > f64::EPSILON
        && current_exposure.0.signum() != latest_desired_exposure.0.signum()
}
