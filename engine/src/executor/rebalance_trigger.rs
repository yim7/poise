use poise_core::types::Exposure;

use crate::runtime::{ExecutionSlot, SlotState};

const MIN_REBALANCE_COMPARISON_TOLERANCE_FACTOR: f64 = 16.0;

pub(super) struct RebalanceTriggerInput<'a> {
    pub current_exposure: &'a Exposure,
    pub latest_target_exposure: &'a Exposure,
    pub min_rebalance_units: f64,
    pub active_slot: Option<&'a ExecutionSlot>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RebalanceTriggerDecision {
    pub anchor: Exposure,
    pub trigger_delta: Exposure,
    pub should_trigger_next_action: bool,
}

pub(super) fn evaluate_rebalance_trigger(
    input: RebalanceTriggerInput<'_>,
) -> RebalanceTriggerDecision {
    let anchor = resolve_anchor(&input);
    let trigger_delta = anchor.delta(input.latest_target_exposure);
    let should_trigger_next_action =
        !trigger_delta_below_min_rebalance_units(&trigger_delta, input.min_rebalance_units);

    RebalanceTriggerDecision {
        anchor,
        trigger_delta,
        should_trigger_next_action,
    }
}

fn resolve_anchor(input: &RebalanceTriggerInput<'_>) -> Exposure {
    match input.active_slot {
        Some(slot)
            if matches!(slot.state, SlotState::SubmitPending | SlotState::Working) =>
        {
            slot.working_order
                .as_ref()
                .map(|order| order.target_exposure.clone())
                .unwrap_or_else(|| input.current_exposure.clone())
        }
        _ => input.current_exposure.clone(),
    }
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
