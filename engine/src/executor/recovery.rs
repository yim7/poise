use chrono::{DateTime, Utc};
use grid_core::types::{ExchangeRules, Exposure};

use crate::grid::{GridId, Instrument};
use crate::observation::OrderObservation;
use crate::ports::OrderRequest;
use crate::runtime::{ExecutionStats, ExecutorState};
use crate::transition::GridEffect;

use super::{
    ExecutionMode, ExecutorInput, PendingSubmitHint, current_submit_hint, recording, slots,
};

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
    pub current_plan: Option<SubmitRecoveryPlanContext<'a>>,
}

#[derive(Debug, Clone)]
pub struct SubmitRecoveryPlanContext<'a> {
    pub grid_id: &'a GridId,
    pub instrument: &'a Instrument,
    pub base_qty_per_unit: f64,
    pub target_exposure: Exposure,
    pub reference_price: f64,
    pub observed_at: DateTime<Utc>,
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
    pub effects: Vec<GridEffect>,
}

pub fn recover_submit_effect(input: SubmitRecoveryInput<'_>) -> SubmitRecoveryPlan {
    if input.previous_state.recovery_anomaly.is_some() {
        return SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::AwaitExchangeState,
            effects: vec![],
        };
    }

    let receipt_backed = input
        .previous_state
        .slots
        .iter()
        .find(|slot| slots::slot_matches_order(slot, &input.request.client_order_id, None))
        .and_then(|slot| slot.working_order.as_ref())
        .and_then(|order| order.order_id.as_ref())
        .is_some();

    if receipt_backed {
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
                    state: recording::clear_pending_submit(
                        input.previous_state,
                        &input.request.client_order_id,
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

    let current_plan_submit = input.current_plan.as_ref().and_then(|current_plan| {
        current_submit_hint(ExecutorInput {
            grid_id: current_plan.grid_id,
            instrument: current_plan.instrument,
            exchange_rules: input.exchange_rules,
            base_qty_per_unit: current_plan.base_qty_per_unit,
            current_exposure: input.current_exposure.clone(),
            target_exposure: current_plan.target_exposure.clone(),
            reference_price: current_plan.reference_price,
            executor_state: None,
            observed_at: current_plan.observed_at,
        })
    });

    if !submit_recovery_matches_current_plan(
        input.request,
        current_plan_submit.as_ref(),
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
                effects: vec![GridEffect::SubmitOrder {
                    request: next_submit.request,
                    target_exposure: next_submit.target_exposure,
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
        .and_then(|_| current_plan_submit.as_ref())
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
    let base_state = input
        .previous_state
        .cloned()
        .unwrap_or_else(|| ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: input
                .current_exposure
                .delta(input.target_exposure.unwrap_or(input.current_exposure)),
            gap_started_at: None,
            last_reprice_at: None,
            slots: vec![slots::empty_inventory_core_slot()],
            last_execution_reason: None,
            recovery_anomaly: None,
            stats: ExecutionStats {
                started_at: input.observed_at,
                max_inventory_gap_abs: Exposure(0.0),
                max_gap_age_ms: 0,
            },
        });

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

fn values_match_with_step(left: f64, right: f64, step: f64) -> bool {
    let tolerance = if step <= f64::EPSILON {
        f64::EPSILON * 16.0
    } else {
        step / 1_000_000.0
    };
    (left - right).abs() <= tolerance
}
