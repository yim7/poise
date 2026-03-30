use serde::{Deserialize, Serialize};

use chrono::{DateTime, Utc};
use grid_core::events::ReplacementGateReason;
use grid_core::types::{ExchangeRules, Exposure, Side};

use crate::execution_plan::ExecutionAction;
use crate::execution_plan::{is_meetable_minimum, round_to_step};
use crate::grid::{GridId, Instrument};
use crate::observation::OrderObservation;
use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};
use crate::runtime::{ExecutionSlot, ExecutionStats, ExecutorState, SlotState, WorkingOrder};
use crate::transition::GridEffect;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Passive,
    Rebalance,
    CatchUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionReason {
    GapEnteredPassive,
    GapEscalatedToRebalance,
    GapEscalatedToCatchUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAnomaly {
    UnknownLiveOrder,
    DuplicateLiveOrders,
    AmbiguousLiveOrder,
}

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
    pub desired_orders: Vec<DesiredOrder>,
    pub effects: Vec<ExecutionAction>,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
}

pub struct RecoveryInput<'a> {
    pub exchange_rules: &'a ExchangeRules,
    pub base_qty_per_unit: f64,
    pub current_exposure: &'a Exposure,
    pub target_exposure: Option<&'a Exposure>,
    pub reference_price: Option<f64>,
    pub previous_state: Option<&'a ExecutorState>,
    pub live_orders: &'a [OrderObservation],
    pub pending_submit_hints: &'a [PendingSubmitHint],
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSubmitHint {
    pub request: OrderRequest,
    pub target_exposure: Exposure,
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

#[derive(Debug, Clone, PartialEq)]
pub enum SubmitReceiptResolution {
    Recorded { state: ExecutorState },
    Unmatched,
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

const REBALANCE_GAP_THRESHOLD: f64 = 2.0;
const CATCH_UP_GAP_THRESHOLD: f64 = 5.0;
const REBALANCE_AGE_MS: i64 = 60_000;
const CATCH_UP_AGE_MS: i64 = 180_000;
const REPLACEMENT_SAFETY_BUFFER_BPS: f64 = 5.0;
const BINANCE_TAKER_FEE_RATE: f64 = 0.0004;
const BPS_DENOMINATOR: f64 = 10_000.0;
pub const INVENTORY_CORE_SLOT: &str = "inventory_core";

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

pub fn record_submit_request(
    previous_state: &ExecutorState,
    request: &OrderRequest,
    target_exposure: Exposure,
) -> ExecutorState {
    let ((_, sibling_slots), _) = split_inventory_core_slot_from_slots(&previous_state.slots);
    let mut state = previous_state.clone();
    state.slots = with_inventory_core_slot(
        sibling_slots,
        ExecutionSlot {
            slot: OrderSlot::new(INVENTORY_CORE_SLOT),
            state: SlotState::SubmitPending,
            working_order: Some(WorkingOrder {
                order_id: None,
                client_order_id: request.client_order_id.clone(),
                side: request.side,
                price: request.price,
                quantity: request.quantity,
                target_exposure,
                status: OrderStatus::Submitting,
                role: role_for_side(request.side),
            }),
        },
    );
    state
}

pub fn record_submit_receipt(
    previous_state: &ExecutorState,
    request: &OrderRequest,
    target_exposure: Exposure,
    receipt: &OrderReceipt,
) -> SubmitReceiptResolution {
    let matching_indexes = previous_state
        .slots
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            slot_matches_order(
                slot,
                &request.client_order_id,
                Some(receipt.order_id.as_str()),
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let [slot_index] = matching_indexes.as_slice() else {
        return SubmitReceiptResolution::Unmatched;
    };
    let slot = &previous_state.slots[*slot_index];
    let Some(existing_order) = slot.working_order.as_ref() else {
        return SubmitReceiptResolution::Unmatched;
    };

    let mut state = previous_state.clone();
    state.slots[*slot_index] = ExecutionSlot {
        slot: slot.slot.clone(),
        state: SlotState::Working,
        working_order: Some(WorkingOrder {
            order_id: Some(receipt.order_id.clone()),
            client_order_id: existing_order.client_order_id.clone(),
            side: request.side,
            price: request.price,
            quantity: request.quantity,
            target_exposure,
            status: receipt.status,
            role: existing_order.role.clone(),
        }),
    };
    SubmitReceiptResolution::Recorded { state }
}

pub fn record_submit_failure(
    previous_state: &ExecutorState,
    client_order_id: &str,
) -> ExecutorState {
    let Some(slots) = clear_matching_slots(&previous_state.slots, |slot| {
        slot.state == SlotState::SubmitPending
            && slot
                .working_order
                .as_ref()
                .is_some_and(|order| order.client_order_id == client_order_id)
    }) else {
        return previous_state.clone();
    };
    let mut state = previous_state.clone();
    state.slots = slots;
    state
}

pub fn apply_order_observation(
    previous_state: &ExecutorState,
    observation: &OrderObservation,
) -> ExecutorState {
    if observation.status.keeps_working_order() {
        let Some(slot) = previous_state.slots.iter().find(|slot| {
            slot_matches_order(
                slot,
                &observation.client_order_id,
                Some(observation.order_id.as_str()),
            )
        }) else {
            return previous_state.clone();
        };
        let Some(existing_order) = slot.working_order.as_ref() else {
            return previous_state.clone();
        };

        let mut state = previous_state.clone();
        state.slots = replace_first_matching_slot(
            &previous_state.slots,
            |candidate| {
                slot_matches_order(
                    candidate,
                    &observation.client_order_id,
                    Some(observation.order_id.as_str()),
                )
            },
            ExecutionSlot {
                slot: slot.slot.clone(),
                state: SlotState::Working,
                working_order: Some(WorkingOrder {
                    order_id: Some(observation.order_id.clone()),
                    client_order_id: observation.client_order_id.clone(),
                    side: observation.side,
                    price: observation.price,
                    quantity: observation.quantity,
                    target_exposure: existing_order.target_exposure.clone(),
                    status: observation.status,
                    role: existing_order.role.clone(),
                }),
            },
        )
        .unwrap_or_else(|| previous_state.slots.clone());
        return state;
    }

    if observation.status.clears_working_order() {
        let Some(slots) = clear_matching_slots(&previous_state.slots, |slot| {
            slot_matches_order(
                slot,
                &observation.client_order_id,
                Some(observation.order_id.as_str()),
            )
        }) else {
            return previous_state.clone();
        };
        let mut state = previous_state.clone();
        state.slots = slots;
        return state;
    }

    previous_state.clone()
}

pub fn clear_pending_submit(
    previous_state: &ExecutorState,
    client_order_id: &str,
) -> ExecutorState {
    let Some(slots) = clear_matching_slots(&previous_state.slots, |slot| {
        slot_matches_order(slot, client_order_id, None)
    }) else {
        return previous_state.clone();
    };
    let mut state = previous_state.clone();
    state.slots = slots;
    state
}

pub fn clear_working_order_by_order_id(
    previous_state: &ExecutorState,
    order_id: &str,
) -> ExecutorState {
    let Some(slots) = clear_matching_slots(&previous_state.slots, |slot| {
        slot_matches_order(slot, "", Some(order_id))
    }) else {
        return previous_state.clone();
    };
    let mut state = previous_state.clone();
    state.slots = slots;
    state
}

pub fn clear_all_working_orders(previous_state: &ExecutorState) -> ExecutorState {
    let Some(slots) = clear_matching_slots(&previous_state.slots, |slot| {
        slot.state == SlotState::Working
    }) else {
        return previous_state.clone();
    };
    let mut state = previous_state.clone();
    state.slots = slots;
    state
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
        .find(|slot| slot_matches_order(slot, &input.request.client_order_id, None))
        .and_then(|slot| slot.working_order.as_ref())
        .and_then(|order| order.order_id.as_ref())
        .is_some();

    if receipt_backed {
        if let Some(live_order) = input.live_order {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Recovered {
                    state: apply_order_observation(input.previous_state, live_order),
                },
                effects: vec![],
            };
        }

        if target_exposure_reached(input.current_exposure, input.target_exposure) {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Recovered {
                    state: clear_pending_submit(
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
            clear_pending_submit(input.previous_state, &input.request.client_order_id);
        if let Some(next_submit) = current_plan_submit {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Superseded {
                    state: record_submit_request(
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
            state: record_submit_request(
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
            slots: vec![empty_inventory_core_slot()],
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
        state.slots = vec![empty_inventory_core_slot()];
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
                slot_matches_order(
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
                rebuild_slot_from_live_order(
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
    let (current_slot, sibling_slots) = split_inventory_core_slot(input.executor_state);
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
                    with_inventory_core_slot(sibling_slots, current_slot),
                    None,
                );
            }
            (
                vec![ExecutionAction::NoOp],
                with_inventory_core_slot(sibling_slots, empty_inventory_core_slot()),
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
                with_inventory_core_slot(
                    sibling_slots,
                    submit_pending_slot(desired_order, &request),
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
                        with_inventory_core_slot(sibling_slots, current_slot),
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
                        with_inventory_core_slot(sibling_slots, current_slot),
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
                        with_inventory_core_slot(sibling_slots, current_slot),
                        None,
                    );
                }
            }

            (
                vec![ExecutionAction::NoOp],
                with_inventory_core_slot(sibling_slots, current_slot),
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
        client_order_id: format!("{}-{}", input.grid_id.as_str(), input.observed_at.timestamp_millis()),
        reduce_only: desired_order.role == OrderRole::DecreaseInventory,
    }
}

fn submit_pending_slot(desired_order: &DesiredOrder, request: &OrderRequest) -> ExecutionSlot {
    ExecutionSlot {
        slot: desired_order.slot.clone(),
        state: SlotState::SubmitPending,
        working_order: Some(WorkingOrder {
            order_id: None,
            client_order_id: request.client_order_id.clone(),
            side: desired_order.side,
            price: desired_order.price,
            quantity: desired_order.quantity,
            target_exposure: desired_order.target_exposure.clone(),
            status: OrderStatus::Submitting,
            role: desired_order.role.clone(),
        }),
    }
}

fn split_inventory_core_slot(
    executor_state: Option<&ExecutorState>,
) -> (ExecutionSlot, Vec<ExecutionSlot>) {
    let Some(executor_state) = executor_state else {
        return (empty_inventory_core_slot(), Vec::new());
    };

    split_inventory_core_slot_from_slots(&executor_state.slots).0
}

fn split_inventory_core_slot_from_slots(
    previous_slots: &[ExecutionSlot],
) -> ((ExecutionSlot, Vec<ExecutionSlot>), bool) {
    let mut current_slot = None;
    let mut sibling_slots = Vec::new();
    for slot in previous_slots {
        if slot.slot.0 == INVENTORY_CORE_SLOT {
            if current_slot.is_none() {
                current_slot = Some(slot.clone());
            }
            continue;
        }
        sibling_slots.push(slot.clone());
    }

    let had_inventory_core = current_slot.is_some();
    (
        (
            current_slot.unwrap_or_else(empty_inventory_core_slot),
            sibling_slots,
        ),
        had_inventory_core,
    )
}

fn with_inventory_core_slot(
    mut sibling_slots: Vec<ExecutionSlot>,
    inventory_core_slot: ExecutionSlot,
) -> Vec<ExecutionSlot> {
    let mut slots = Vec::with_capacity(sibling_slots.len() + 1);
    slots.push(inventory_core_slot);
    slots.append(&mut sibling_slots);
    slots
}

fn replace_first_matching_slot<F>(
    previous_slots: &[ExecutionSlot],
    matcher: F,
    new_slot: ExecutionSlot,
) -> Option<Vec<ExecutionSlot>>
where
    F: Fn(&ExecutionSlot) -> bool,
{
    let mut slots = previous_slots.to_vec();
    let index = slots.iter().position(matcher)?;
    slots[index] = new_slot;
    Some(slots)
}

fn clear_matching_slots<F>(
    previous_slots: &[ExecutionSlot],
    matcher: F,
) -> Option<Vec<ExecutionSlot>>
where
    F: Fn(&ExecutionSlot) -> bool,
{
    let ((inventory_core_slot, sibling_slots), had_inventory_core) =
        split_inventory_core_slot_from_slots(previous_slots);
    let mut changed = !had_inventory_core;
    let inventory_core_slot = if matcher(&inventory_core_slot) {
        changed = true;
        empty_slot(&inventory_core_slot.slot)
    } else {
        inventory_core_slot
    };
    let sibling_slots = sibling_slots
        .into_iter()
        .filter(|slot| {
            let matches = matcher(slot);
            changed |= matches;
            !matches
        })
        .collect::<Vec<_>>();
    changed.then_some(with_inventory_core_slot(sibling_slots, inventory_core_slot))
}

fn slot_matches_order(slot: &ExecutionSlot, client_order_id: &str, order_id: Option<&str>) -> bool {
    let Some(order) = slot.working_order.as_ref() else {
        return false;
    };

    if !client_order_id.is_empty() && order.client_order_id != client_order_id {
        return false;
    }

    match order_id {
        Some(order_id) => match order.order_id.as_deref() {
            Some(existing_order_id) => existing_order_id == order_id,
            None => !client_order_id.is_empty(),
        },
        None => !client_order_id.is_empty(),
    }
}

fn rebuild_slot_from_live_order(
    slot: &ExecutionSlot,
    live_order: &OrderObservation,
    target_exposure: Option<&Exposure>,
    current_exposure: &Exposure,
) -> ExecutionSlot {
    let target_exposure = slot
        .working_order
        .as_ref()
        .map(|order| order.target_exposure.clone())
        .or_else(|| target_exposure.cloned())
        .unwrap_or_else(|| current_exposure.clone());
    let role = slot
        .working_order
        .as_ref()
        .map(|order| order.role.clone())
        .unwrap_or_else(|| role_for_side(live_order.side));

    ExecutionSlot {
        slot: slot.slot.clone(),
        state: SlotState::Working,
        working_order: Some(WorkingOrder {
            order_id: Some(live_order.order_id.clone()),
            client_order_id: live_order.client_order_id.clone(),
            side: live_order.side,
            price: live_order.price,
            quantity: live_order.quantity,
            target_exposure,
            status: live_order.status,
            role,
        }),
    }
}

fn recovery_anomaly(base_state: &ExecutorState, anomaly: RecoveryAnomaly) -> RecoveryResolution {
    let mut state = base_state.clone();
    state.slots = vec![empty_inventory_core_slot()];
    state.recovery_anomaly = Some(anomaly.clone());
    RecoveryResolution::Anomaly { state, anomaly }
}

fn empty_inventory_core_slot() -> ExecutionSlot {
    empty_slot(&OrderSlot::new(INVENTORY_CORE_SLOT))
}

fn empty_slot(slot: &OrderSlot) -> ExecutionSlot {
    ExecutionSlot {
        slot: slot.clone(),
        state: SlotState::Empty,
        working_order: None,
    }
}

fn role_for_side(side: Side) -> OrderRole {
    match side {
        Side::Buy => OrderRole::IncreaseInventory,
        Side::Sell => OrderRole::DecreaseInventory,
    }
}

fn target_exposure_reached(current_exposure: &Exposure, target_exposure: &Exposure) -> bool {
    let delta = target_exposure.0 - current_exposure.0;
    if delta.abs() <= f64::EPSILON {
        return true;
    }

    if target_exposure.0.abs() <= f64::EPSILON {
        return current_exposure.0.abs() <= f64::EPSILON;
    }

    if target_exposure.0 >= 0.0 {
        current_exposure.0 >= target_exposure.0
    } else {
        current_exposure.0 <= target_exposure.0
    }
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

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::grid::GridId;
    use crate::grid::Venue;
    use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};
    use crate::runtime::{ExecutionSlot, ExecutionStats, SlotState, WorkingOrder};

    fn test_grid_id() -> GridId {
        GridId::new("btc-core")
    }

    fn test_instrument() -> Instrument {
        Instrument::new(Venue::Binance, "BTCUSDT")
    }

    fn test_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
        }
    }

    fn test_executor_state(
        mode: ExecutionMode,
        gap_started_at: Option<DateTime<Utc>>,
    ) -> ExecutorState {
        ExecutorState {
            mode,
            inventory_gap: Exposure(4.0),
            gap_started_at,
            last_reprice_at: None,
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(WorkingOrder {
                    order_id: Some("order-1".into()),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    target_exposure: Exposure(4.0),
                    status: OrderStatus::New,
                    role: OrderRole::IncreaseInventory,
                }),
            }],
            last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
            recovery_anomaly: None,
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
                max_inventory_gap_abs: Exposure(4.0),
                max_gap_age_ms: 60_000,
            },
        }
    }

    fn sibling_slot() -> ExecutionSlot {
        ExecutionSlot {
            slot: OrderSlot::new("inventory_followup"),
            state: SlotState::Working,
            working_order: Some(WorkingOrder {
                order_id: Some("order-2".into()),
                client_order_id: "client-2".into(),
                side: Side::Sell,
                price: 96.0,
                quantity: 12.0,
                target_exposure: Exposure(2.0),
                status: OrderStatus::PartiallyFilled,
                role: OrderRole::DecreaseInventory,
            }),
        }
    }

    #[test]
    fn plans_execution_mode_from_gap_and_age() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let passive = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(1.0),
            reference_price: 99.9,
            executor_state: None,
            observed_at: now,
        });
        assert_eq!(passive.state.mode, ExecutionMode::Passive);
        assert_eq!(
            passive.state.last_execution_reason,
            Some(ExecutionReason::GapEnteredPassive)
        );

        let rebalance = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(3.0),
            reference_price: 99.9,
            executor_state: Some(&test_executor_state(
                ExecutionMode::Passive,
                Some(now - Duration::seconds(90)),
            )),
            observed_at: now,
        });
        assert_eq!(rebalance.state.mode, ExecutionMode::Rebalance);
        assert_eq!(
            rebalance.state.last_execution_reason,
            Some(ExecutionReason::GapEscalatedToRebalance)
        );

        let catch_up = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(6.0),
            reference_price: 99.9,
            executor_state: Some(&test_executor_state(
                ExecutionMode::Rebalance,
                Some(now - Duration::seconds(240)),
            )),
            observed_at: now,
        });
        assert_eq!(catch_up.state.mode, ExecutionMode::CatchUp);
        assert_eq!(
            catch_up.state.last_execution_reason,
            Some(ExecutionReason::GapEscalatedToCatchUp)
        );
        assert_eq!(catch_up.desired_orders.len(), 1);
        assert!(
            catch_up.state.stats.max_inventory_gap_abs.0 >= catch_up.state.inventory_gap.0.abs()
        );
    }

    #[test]
    fn cancel_plan_keeps_live_slot_until_cancel_effect_completes() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let mut existing_state = test_executor_state(ExecutionMode::Passive, Some(now));
        existing_state.slots.push(sibling_slot());

        let plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(4.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: Some(&existing_state),
            observed_at: now,
        });

        assert!(matches!(
            plan.effects.as_slice(),
            [ExecutionAction::CancelOrder { order_id, .. }] if order_id == "order-1"
        ));
        assert_eq!(plan.state.slots, existing_state.slots);
    }

    #[test]
    fn replace_plan_keeps_live_slot_until_cancel_effect_completes() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = test_executor_state(ExecutionMode::Passive, Some(now));

        let plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 90.0,
            executor_state: Some(&existing_state),
            observed_at: now,
        });

        assert!(matches!(
            plan.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { order_id, .. },
                ExecutionAction::SubmitOrder { .. }
            ] if order_id == "order-1"
        ));
        assert_eq!(plan.state.slots, existing_state.slots);
    }

    #[test]
    fn current_submit_hint_returns_single_submit_effect_from_plan() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let hint = current_submit_hint(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: now,
        });

        let hint = hint.expect("expected current plan to expose a single submit hint");
        assert_eq!(hint.request.instrument, instrument);
        assert_eq!(
            hint.request.client_order_id,
            format!("btc-core-{}", now.timestamp_millis())
        );
        assert!(!hint.request.reduce_only);
        assert_eq!(hint.request.side, Side::Buy);
        assert_eq!(hint.request.price, 95.0);
        assert_eq!(hint.request.quantity, 15.0);
        assert_eq!(hint.target_exposure, Exposure(4.0));
    }

    #[test]
    fn current_submit_hint_returns_none_when_plan_is_not_single_submit() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = test_executor_state(ExecutionMode::Passive, Some(now));

        let hint = current_submit_hint(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 90.0,
            executor_state: Some(&existing_state),
            observed_at: now,
        });

        assert!(hint.is_none());
    }

    #[test]
    fn plan_sets_reduce_only_for_decrease_inventory_order() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(6.0),
            target_exposure: Exposure(2.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: now,
        });

        let submit = plan.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request),
            _ => None,
        });
        assert!(submit.is_some());
        assert!(submit.unwrap().reduce_only);
    }

    #[test]
    fn submit_receipt_promotes_submit_pending_slot_to_working() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };

        let pending = record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0));
        assert_eq!(pending.slots.len(), 1);
        assert_eq!(pending.slots[0].slot, OrderSlot::new("inventory_core"));
        assert_eq!(pending.slots[0].state, SlotState::SubmitPending);
        assert_eq!(
            pending.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            None
        );

        let SubmitReceiptResolution::Recorded { state: working } = record_submit_receipt(
            &pending,
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        ) else {
            panic!("expected matching submit receipt to promote slot");
        };
        assert_eq!(working.slots.len(), 1);
        assert_eq!(working.slots[0].state, SlotState::Working);
        assert_eq!(
            working.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-1")
        );
        assert_eq!(
            working.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.status),
            Some(OrderStatus::New)
        );
    }

    #[test]
    fn submit_receipt_without_matching_slot_is_rejected() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let mut state = ExecutorState::empty(now);
        state.slots.push(sibling_slot());

        let resolution = record_submit_receipt(
            &state,
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        );

        assert!(matches!(resolution, SubmitReceiptResolution::Unmatched));
    }

    #[test]
    fn submit_receipt_requires_matching_order_id_once_slot_is_receipt_backed() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: receipt_backed,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };

        let resolution = record_submit_receipt(
            &receipt_backed,
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-2".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        );

        assert!(matches!(resolution, SubmitReceiptResolution::Unmatched));
    }

    #[test]
    fn submit_receipt_is_rejected_when_multiple_slots_match_same_client_order_id() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let mut state = record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0));
        state.slots.push(ExecutionSlot {
            slot: OrderSlot::new("inventory_followup"),
            state: SlotState::SubmitPending,
            working_order: Some(WorkingOrder {
                order_id: None,
                client_order_id: "client-1".into(),
                side: Side::Sell,
                price: 96.0,
                quantity: 12.0,
                target_exposure: Exposure(2.0),
                status: OrderStatus::Submitting,
                role: OrderRole::DecreaseInventory,
            }),
        });

        let resolution = record_submit_receipt(
            &state,
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        );

        assert!(matches!(resolution, SubmitReceiptResolution::Unmatched));
    }

    #[test]
    fn submit_failure_does_not_clear_receipt_backed_slot_with_same_client_order_id() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: receipt_backed,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };

        let next_state = record_submit_failure(&receipt_backed, &request.client_order_id);

        assert_eq!(next_state, receipt_backed);
    }

    #[test]
    fn terminal_order_clears_matching_slot_to_empty() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded { state: working } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        ) else {
            panic!("expected initial receipt to be recorded");
        };

        let cleared = apply_order_observation(
            &working,
            &OrderObservation {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::Filled,
            },
        );
        assert_eq!(
            cleared.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Empty,
                working_order: None,
            }]
        );

        let unchanged = apply_order_observation(
            &working,
            &OrderObservation {
                order_id: "order-2".into(),
                client_order_id: "client-2".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::Filled,
            },
        );
        assert_eq!(unchanged.slots, working.slots);
    }

    #[test]
    fn recovery_marks_unknown_live_order_when_no_slot_can_be_inferred() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let recovery = recover_working_orders(RecoveryInput {
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: &Exposure(0.0),
            target_exposure: None,
            reference_price: None,
            previous_state: Some(&ExecutorState::empty(now)),
            live_orders: &[OrderObservation {
                order_id: "live-1".into(),
                client_order_id: "unexpected-live".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                realized_pnl: 0.0,
                status: OrderStatus::New,
            }],
            pending_submit_hints: &[],
            observed_at: now,
        });

        assert!(matches!(
            recovery,
            RecoveryResolution::Anomaly {
                anomaly: RecoveryAnomaly::UnknownLiveOrder,
                ..
            }
        ));
    }

    #[test]
    fn recovery_marks_unknown_live_order_without_historical_slot_even_when_target_exists() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let recovery = recover_working_orders(RecoveryInput {
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: &Exposure(2.0),
            target_exposure: Some(&Exposure(4.0)),
            reference_price: Some(95.0),
            previous_state: Some(&ExecutorState::empty(now)),
            live_orders: &[OrderObservation {
                order_id: "live-1".into(),
                client_order_id: "unexpected-live".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                realized_pnl: 0.0,
                status: OrderStatus::New,
            }],
            pending_submit_hints: &[],
            observed_at: now,
        });

        assert!(matches!(
            recovery,
            RecoveryResolution::Anomaly {
                anomaly: RecoveryAnomaly::UnknownLiveOrder,
                ..
            }
        ));
    }

    #[test]
    fn recovery_rebuilds_multiple_live_orders_into_distinct_slots() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let mut previous_state = test_executor_state(ExecutionMode::Passive, Some(now));
        previous_state.slots.push(sibling_slot());

        let recovery = recover_working_orders(RecoveryInput {
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: &Exposure(2.0),
            target_exposure: Some(&Exposure(4.0)),
            reference_price: Some(95.0),
            previous_state: Some(&previous_state),
            live_orders: &[
                OrderObservation {
                    order_id: "order-2".into(),
                    client_order_id: "client-2".into(),
                    side: Side::Sell,
                    price: 96.0,
                    quantity: 12.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                },
                OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::PartiallyFilled,
                },
            ],
            pending_submit_hints: &[],
            observed_at: now,
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected two uniquely matched live orders to be rebuilt");
        };
        assert!(state.recovery_anomaly.is_none());
        assert_eq!(state.slots.len(), 2);
        assert_eq!(state.slots[0].slot, OrderSlot::new("inventory_core"));
        assert_eq!(
            state.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-1")
        );
        assert_eq!(
            state.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.status),
            Some(OrderStatus::PartiallyFilled)
        );
        assert_eq!(state.slots[1].slot, OrderSlot::new("inventory_followup"));
        assert_eq!(
            state.slots[1]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-2")
        );
        assert_eq!(
            state.slots[1]
                .working_order
                .as_ref()
                .map(|order| order.status),
            Some(OrderStatus::New)
        );
    }

    #[test]
    fn recovery_returns_anomaly_state_when_multiple_live_orders_claim_same_slot() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0));

        let recovery = recover_working_orders(RecoveryInput {
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: &Exposure(0.0),
            target_exposure: Some(&Exposure(4.0)),
            reference_price: Some(95.0),
            previous_state: Some(&previous_state),
            live_orders: &[
                OrderObservation {
                    order_id: "live-1".into(),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                },
                OrderObservation {
                    order_id: "live-2".into(),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::PartiallyFilled,
                },
            ],
            pending_submit_hints: &[],
            observed_at: now,
        });

        let RecoveryResolution::Anomaly { state, anomaly } = recovery else {
            panic!("expected duplicate live orders on one slot to raise anomaly");
        };
        assert_eq!(anomaly, RecoveryAnomaly::DuplicateLiveOrders);
        assert_eq!(state.slots, vec![empty_inventory_core_slot()]);
        assert_eq!(
            state.recovery_anomaly.as_ref(),
            Some(&RecoveryAnomaly::DuplicateLiveOrders)
        );
    }

    #[test]
    fn submit_recovery_restores_live_order_from_receipt_backed_slot() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: previous_state,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };
        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(4.0),
            current_exposure: &Exposure(0.0),
            live_order: Some(&OrderObservation {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::PartiallyFilled,
            }),
            current_plan: None,
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Recovered { state },
            effects,
        } = recovery
        else {
            panic!("expected receipt-backed live order to be recovered");
        };
        assert!(effects.is_empty());
        assert_eq!(state.slots.len(), 1);
        assert_eq!(state.slots[0].state, SlotState::Working);
        assert_eq!(
            state.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.status),
            Some(OrderStatus::PartiallyFilled)
        );
    }

    #[test]
    fn submit_recovery_supersedes_stale_effect_when_current_plan_changed() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let grid_id = GridId::new("grid-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 94.0,
            quantity: 22.5,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(6.0));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(6.0),
            current_exposure: &Exposure(0.0),
            live_order: None,
            current_plan: Some(SubmitRecoveryPlanContext {
                grid_id: &grid_id,
                instrument: &instrument,
                base_qty_per_unit: 3.75,
                target_exposure: Exposure(4.0),
                reference_price: 95.0,
                observed_at: now,
            }),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded { state },
            effects,
        } = recovery
        else {
            panic!("expected stale submit effect to be superseded");
        };
        assert_eq!(
            effects,
            vec![ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument,
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    client_order_id: format!("grid-1-{}", now.timestamp_millis()),
                    reduce_only: false,
                },
                target_exposure: Exposure(4.0),
            }]
        );
        assert_eq!(
            state.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::SubmitPending,
                working_order: Some(WorkingOrder {
                    order_id: None,
                    client_order_id: format!("grid-1-{}", now.timestamp_millis()),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    target_exposure: Exposure(4.0),
                    status: OrderStatus::Submitting,
                    role: OrderRole::IncreaseInventory,
                }),
            }]
        );
    }

    #[test]
    fn submit_requests_match_rejects_different_reduce_only_semantics() {
        let rules = test_exchange_rules();
        let left = OrderRequest {
            instrument: test_instrument(),
            side: Side::Sell,
            price: 100.0,
            quantity: 3.8,
            client_order_id: "client-1".into(),
            reduce_only: true,
        };
        let right = OrderRequest {
            instrument: test_instrument(),
            side: Side::Sell,
            price: 100.0,
            quantity: 3.8,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };

        assert!(!submit_requests_match(&left, &right, &rules));
    }

    #[test]
    fn submit_requests_match_ignores_client_order_id() {
        let rules = test_exchange_rules();
        let left = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 3.8,
            client_order_id: "btc-core-1711699500000".into(),
            reduce_only: false,
        };
        let right = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 3.8,
            client_order_id: "btc-core-1711699500050".into(),
            reduce_only: false,
        };

        assert!(submit_requests_match(&left, &right, &rules));
    }

    #[test]
    fn plan_generates_unique_client_order_ids_across_calls() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let t1 = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let t2 = t1 + Duration::milliseconds(1);

        let plan1 = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: t1,
        });
        let plan2 = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: t2,
        });

        let id1 = plan1.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request.client_order_id.clone()),
            _ => None,
        });
        let id2 = plan2.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request.client_order_id.clone()),
            _ => None,
        });

        assert!(id1.is_some());
        assert!(id2.is_some());
        assert_ne!(id1, id2);
        assert!(id1.unwrap().starts_with("btc-core-"));
    }

    #[test]
    fn submit_recovery_proceed_updates_slot_target_to_current_plan_target() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let grid_id = GridId::new("grid-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 4.0,
            client_order_id: "grid-1-reconcile".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(6.0));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(6.0),
            current_exposure: &Exposure(0.0),
            live_order: None,
            current_plan: Some(SubmitRecoveryPlanContext {
                grid_id: &grid_id,
                instrument: &instrument,
                base_qty_per_unit: 1.0,
                target_exposure: Exposure(4.0),
                reference_price: 90.0,
                observed_at: now,
            }),
        });

        let SubmitRecoveryPlan {
            resolution:
                SubmitRecoveryResolution::Proceed {
                    state,
                    target_exposure,
                },
            effects,
        } = recovery
        else {
            panic!("expected matching request to keep proceed resolution");
        };
        assert!(effects.is_empty());
        assert_eq!(target_exposure, Exposure(4.0));
        assert_eq!(
            state.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.target_exposure.clone()),
            Some(Exposure(4.0))
        );
    }

    #[test]
    fn recovery_clears_receipt_backed_slot_without_matching_pending_submit_effect() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: previous_state,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };

        let recovery = recover_working_orders(RecoveryInput {
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: &Exposure(2.0),
            target_exposure: Some(&Exposure(4.0)),
            reference_price: Some(95.0),
            previous_state: Some(&previous_state),
            live_orders: &[],
            pending_submit_hints: &[],
            observed_at: now,
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected stale receipt-backed slot to be cleared");
        };
        assert_eq!(state.slots, vec![empty_inventory_core_slot()]);
        assert!(state.recovery_anomaly.is_none());
    }

    #[test]
    fn recovery_marks_anomaly_when_pending_receipt_backed_slot_has_no_live_order() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: previous_state,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };
        let pending_submit_hints = vec![PendingSubmitHint {
            request: request.clone(),
            target_exposure: Exposure(4.0),
        }];

        let recovery = recover_working_orders(RecoveryInput {
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: &Exposure(2.0),
            target_exposure: Some(&Exposure(4.0)),
            reference_price: Some(95.0),
            previous_state: Some(&previous_state),
            live_orders: &[],
            pending_submit_hints: &pending_submit_hints,
            observed_at: now,
        });

        assert!(matches!(
            recovery,
            RecoveryResolution::Anomaly {
                anomaly: RecoveryAnomaly::UnknownLiveOrder,
                ..
            }
        ));
    }
}
