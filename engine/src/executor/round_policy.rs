use chrono::{DateTime, Utc};
use poise_core::types::{Exposure, Side};

use crate::runtime::{ExecutionRound, ExecutionStats, ExecutorState, SlotState};

use super::rebalance_trigger::{
    ActiveLifecycle, RebalanceTriggerDecision, RebalanceTriggerInput, evaluate_rebalance_trigger,
};
use super::{ExecutionMode, ExecutionReason, OrderSlot};

const REBALANCE_GAP_THRESHOLD: f64 = 2.0;
const CATCH_UP_GAP_THRESHOLD: f64 = 5.0;
const REBALANCE_AGE_MS: i64 = 60_000;
const CATCH_UP_AGE_MS: i64 = 180_000;

#[derive(Debug, Clone, PartialEq)]
pub struct RoundPolicySlotSummary {
    pub slot: OrderSlot,
    pub phase: SlotState,
    pub working_side: Option<Side>,
    pub working_price: Option<f64>,
    pub working_quantity: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoundPolicyActiveRound<'a> {
    pub target_exposure: &'a Exposure,
    pub mode: ExecutionMode,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundLifecycleDecision {
    StartRound,
    ContinueRound,
    SwitchRound,
    FinishRound,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoundPolicyInput<'a> {
    pub current_exposure: &'a Exposure,
    pub desired_exposure: &'a Exposure,
    pub active_round: Option<RoundPolicyActiveRound<'a>>,
    pub slots: Vec<RoundPolicySlotSummary>,
    pub observed_at: DateTime<Utc>,
    min_rebalance_units: f64,
    previous_mode: Option<ExecutionMode>,
    previous_last_execution_reason: Option<ExecutionReason>,
    previous_inventory_gap: Option<Exposure>,
    previous_gap_started_at: Option<DateTime<Utc>>,
    previous_stats: Option<&'a ExecutionStats>,
    active_lifecycle: ActiveLifecycle<'a>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoundDecision {
    pub active_round: Option<ExecutionRound>,
    pub lifecycle: RoundLifecycleDecision,
    pub(super) trigger_decision: RebalanceTriggerDecision,
    pub inventory_gap: Exposure,
    pub gap_started_at: Option<DateTime<Utc>>,
    pub mode: ExecutionMode,
    pub last_execution_reason: Option<ExecutionReason>,
    pub stats: ExecutionStats,
}

pub fn round_policy_input_from_state<'a>(
    current_exposure: &'a Exposure,
    desired_exposure: &'a Exposure,
    previous_state: Option<&'a ExecutorState>,
    min_rebalance_units: f64,
    observed_at: DateTime<Utc>,
) -> RoundPolicyInput<'a> {
    round_policy_input_from_state_with_lifecycle(
        current_exposure,
        desired_exposure,
        previous_state,
        None,
        min_rebalance_units,
        observed_at,
        None,
    )
}

pub(super) fn round_policy_input_from_state_with_lifecycle<'a>(
    current_exposure: &'a Exposure,
    desired_exposure: &'a Exposure,
    previous_state: Option<&'a ExecutorState>,
    active_round: Option<RoundPolicyActiveRound<'a>>,
    min_rebalance_units: f64,
    observed_at: DateTime<Utc>,
    active_lifecycle: Option<ActiveLifecycle<'a>>,
) -> RoundPolicyInput<'a> {
    RoundPolicyInput {
        current_exposure,
        desired_exposure,
        active_round: active_round.or_else(|| {
            previous_state.and_then(|state| {
                state.active_round.as_ref().map(|active_round| RoundPolicyActiveRound {
                    target_exposure: &active_round.target_exposure,
                    mode: active_round.mode.clone(),
                    started_at: active_round.started_at,
                })
            })
        }),
        slots: previous_state
            .map(|state| {
                state
                    .slots
                    .iter()
                    .map(|slot| RoundPolicySlotSummary {
                        slot: slot.slot.clone(),
                        phase: slot.state.clone(),
                        working_side: slot.working_order.as_ref().map(|order| order.side),
                        working_price: slot.working_order.as_ref().map(|order| order.price),
                        working_quantity: slot.working_order.as_ref().map(|order| order.quantity),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        observed_at,
        min_rebalance_units,
        previous_mode: previous_state.map(|state| state.diagnostics.mode.clone()),
        previous_last_execution_reason: previous_state
            .and_then(|state| state.diagnostics.last_execution_reason.clone()),
        previous_inventory_gap: previous_state.map(|state| state.diagnostics.inventory_gap.clone()),
        previous_gap_started_at: previous_state.and_then(|state| state.diagnostics.gap_started_at),
        previous_stats: previous_state.map(|state| &state.stats),
        active_lifecycle: active_lifecycle
            .unwrap_or_else(|| ActiveLifecycle::from_executor_state(previous_state)),
    }
}

pub fn evaluate_round_policy(input: RoundPolicyInput<'_>) -> RoundDecision {
    let inventory_gap = input.current_exposure.delta(input.desired_exposure);
    let gap_started_at = resolve_gap_started_at(
        input.previous_inventory_gap.as_ref(),
        input.previous_gap_started_at,
        &inventory_gap,
        input.observed_at,
    );
    let gap_age_ms = gap_started_at
        .map(|started_at| (input.observed_at - started_at).num_milliseconds().max(0))
        .unwrap_or(0);
    let mode = resolve_mode(&inventory_gap, gap_age_ms);
    let last_execution_reason = resolve_reason(
        input.previous_mode.as_ref(),
        input.previous_last_execution_reason.as_ref(),
        &mode,
    );
    let stats = update_stats(input.previous_stats, input.observed_at, &inventory_gap, gap_age_ms);
    let trigger_decision = evaluate_rebalance_trigger(RebalanceTriggerInput {
        current_exposure: input.current_exposure,
        latest_target_exposure: input.desired_exposure,
        active_round_target_exposure: input.active_round.as_ref().map(|round| round.target_exposure),
        min_rebalance_units: input.min_rebalance_units,
        active_lifecycle: input.active_lifecycle,
    });
    let has_active_round = input.active_round.is_some();
    let lifecycle = match trigger_decision {
        RebalanceTriggerDecision::PreserveActiveLifecycle if has_active_round => {
            RoundLifecycleDecision::ContinueRound
        }
        RebalanceTriggerDecision::PreserveActiveLifecycle => RoundLifecycleDecision::StartRound,
        RebalanceTriggerDecision::TriggerFreshAction if has_active_round => {
            RoundLifecycleDecision::SwitchRound
        }
        RebalanceTriggerDecision::TriggerFreshAction => RoundLifecycleDecision::StartRound,
        RebalanceTriggerDecision::Suppress => RoundLifecycleDecision::FinishRound,
    };
    let active_round = match lifecycle {
        RoundLifecycleDecision::ContinueRound => input.active_round.map(|active_round| {
            ExecutionRound {
                target_exposure: active_round.target_exposure.clone(),
                mode: mode.clone(),
                started_at: active_round.started_at,
            }
        }),
        RoundLifecycleDecision::StartRound | RoundLifecycleDecision::SwitchRound => {
            Some(ExecutionRound {
                target_exposure: input.desired_exposure.clone(),
                mode: mode.clone(),
                started_at: input.observed_at,
            })
        }
        RoundLifecycleDecision::FinishRound => None,
    };

    RoundDecision {
        active_round,
        lifecycle,
        trigger_decision,
        inventory_gap,
        gap_started_at,
        mode,
        last_execution_reason,
        stats,
    }
}

fn resolve_gap_started_at(
    previous_inventory_gap: Option<&Exposure>,
    previous_gap_started_at: Option<DateTime<Utc>>,
    inventory_gap: &Exposure,
    observed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if inventory_gap.is_zero() {
        return None;
    }

    previous_inventory_gap
        .and_then(|previous_gap| {
            (!previous_gap.is_zero() && previous_gap.0.signum() == inventory_gap.0.signum())
                .then_some(previous_gap_started_at)
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
    previous_mode: Option<&ExecutionMode>,
    previous_last_execution_reason: Option<&ExecutionReason>,
    mode: &ExecutionMode,
) -> Option<ExecutionReason> {
    if previous_mode == Some(mode) {
        return previous_last_execution_reason.cloned();
    }

    Some(match mode {
        ExecutionMode::Passive => ExecutionReason::GapEnteredPassive,
        ExecutionMode::Rebalance => ExecutionReason::GapEscalatedToRebalance,
        ExecutionMode::CatchUp => ExecutionReason::GapEscalatedToCatchUp,
    })
}

fn update_stats(
    previous_stats: Option<&ExecutionStats>,
    observed_at: DateTime<Utc>,
    inventory_gap: &Exposure,
    gap_age_ms: i64,
) -> ExecutionStats {
    let started_at = previous_stats
        .map(|stats| stats.started_at)
        .unwrap_or(observed_at);
    let previous_max_gap = previous_stats
        .map(|stats| stats.max_inventory_gap_abs.0.abs())
        .unwrap_or(0.0);
    let previous_max_age = previous_stats.map(|stats| stats.max_gap_age_ms).unwrap_or(0);

    ExecutionStats {
        started_at,
        max_inventory_gap_abs: Exposure(previous_max_gap.max(inventory_gap.0.abs())),
        max_gap_age_ms: previous_max_age.max(gap_age_ms),
    }
}
