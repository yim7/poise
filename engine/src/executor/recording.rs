use crate::observation::OrderObservation;
use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};
use crate::runtime::{ExecutionSlot, ExecutorState, RecentTerminalOrder, SlotState, WorkingOrder};

use super::{DesiredOrder, INVENTORY_CORE_SLOT, OrderSlot, slots};

const RECENT_TERMINAL_ORDER_LIMIT: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub enum SubmitReceiptResolution {
    Recorded { state: ExecutorState },
    Unmatched,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderUpdateAbsorbResult {
    Applied,
    DuplicateReplay,
    Unabsorbed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderObservationApplication {
    pub state: ExecutorState,
    pub absorb_result: OrderUpdateAbsorbResult,
}

pub fn record_submit_request(
    previous_state: &ExecutorState,
    request: &OrderRequest,
    target_exposure: poise_core::types::Exposure,
) -> ExecutorState {
    let ((_, sibling_slots), _) =
        slots::split_inventory_core_slot_from_slots(&previous_state.slots);
    let mut state = previous_state.clone();
    state.slots = slots::with_inventory_core_slot(
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
                role: slots::role_for_reduce_only(request.reduce_only),
            }),
        },
    );
    state
}

fn remember_terminal_order(state: &mut ExecutorState, client_order_id: &str, order_id: &str) {
    if order_id.is_empty() {
        return;
    }

    let marker = RecentTerminalOrder {
        client_order_id: client_order_id.to_string(),
        order_id: order_id.to_string(),
    };
    state
        .recent_terminal_orders
        .retain(|existing| existing != &marker);
    state.recent_terminal_orders.push(marker);
    if state.recent_terminal_orders.len() > RECENT_TERMINAL_ORDER_LIMIT {
        let overflow = state.recent_terminal_orders.len() - RECENT_TERMINAL_ORDER_LIMIT;
        state.recent_terminal_orders.drain(0..overflow);
    }
}

fn remember_terminal_orders_from_slots(state: &mut ExecutorState, slots: &[ExecutionSlot]) {
    for slot in slots {
        let Some(order) = slot.working_order.as_ref() else {
            continue;
        };
        let Some(order_id) = order.order_id.as_deref() else {
            continue;
        };
        remember_terminal_order(state, &order.client_order_id, order_id);
    }
}

fn is_recent_terminal_order(
    previous_state: &ExecutorState,
    observation: &OrderObservation,
) -> bool {
    previous_state.recent_terminal_orders.iter().any(|recent| {
        recent.client_order_id == observation.client_order_id
            && recent.order_id == observation.order_id
    })
}

pub fn record_submit_receipt(
    previous_state: &ExecutorState,
    request: &OrderRequest,
    target_exposure: poise_core::types::Exposure,
    receipt: &OrderReceipt,
) -> SubmitReceiptResolution {
    let matching_indexes = previous_state
        .slots
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            slots::slot_matches_order(
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
    let Some(slots) = slots::clear_matching_slots(&previous_state.slots, |slot| {
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
    apply_order_observation_with_result(previous_state, observation).state
}

pub fn apply_order_observation_with_result(
    previous_state: &ExecutorState,
    observation: &OrderObservation,
) -> OrderObservationApplication {
    if observation.status.keeps_working_order() {
        let matching_indexes = previous_state
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slots::slot_matches_order(
                    slot,
                    &observation.client_order_id,
                    Some(observation.order_id.as_str()),
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let [slot_index] = matching_indexes.as_slice() else {
            return OrderObservationApplication {
                state: previous_state.clone(),
                absorb_result: OrderUpdateAbsorbResult::Unabsorbed,
            };
        };
        let slot = &previous_state.slots[*slot_index];
        let Some(existing_order) = slot.working_order.as_ref() else {
            return OrderObservationApplication {
                state: previous_state.clone(),
                absorb_result: OrderUpdateAbsorbResult::Unabsorbed,
            };
        };

        let mut state = previous_state.clone();
        state.slots = slots::replace_first_matching_slot(
            &previous_state.slots,
            |candidate| {
                slots::slot_matches_order(
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
        let absorb_result = if state == *previous_state {
            OrderUpdateAbsorbResult::DuplicateReplay
        } else {
            OrderUpdateAbsorbResult::Applied
        };
        return OrderObservationApplication {
            state,
            absorb_result,
        };
    }

    if observation.status.clears_working_order() {
        let matching_indexes = previous_state
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slots::slot_matches_order(
                    slot,
                    &observation.client_order_id,
                    Some(observation.order_id.as_str()),
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if matching_indexes.len() != 1 {
            let absorb_result = if is_recent_terminal_order(previous_state, observation) {
                OrderUpdateAbsorbResult::DuplicateReplay
            } else {
                OrderUpdateAbsorbResult::Unabsorbed
            };
            return OrderObservationApplication {
                state: previous_state.clone(),
                absorb_result,
            };
        }
        let Some(slots) = slots::clear_matching_slots(&previous_state.slots, |slot| {
            slots::slot_matches_order(
                slot,
                &observation.client_order_id,
                Some(observation.order_id.as_str()),
            )
        }) else {
            return OrderObservationApplication {
                state: previous_state.clone(),
                absorb_result: OrderUpdateAbsorbResult::Unabsorbed,
            };
        };
        let mut state = previous_state.clone();
        state.slots = slots;
        remember_terminal_order(
            &mut state,
            &observation.client_order_id,
            &observation.order_id,
        );
        return OrderObservationApplication {
            state,
            absorb_result: OrderUpdateAbsorbResult::Applied,
        };
    }

    OrderObservationApplication {
        state: previous_state.clone(),
        absorb_result: OrderUpdateAbsorbResult::DuplicateReplay,
    }
}

pub fn clear_pending_submit(
    previous_state: &ExecutorState,
    client_order_id: &str,
) -> ExecutorState {
    let Some(slots) = slots::clear_matching_slots(&previous_state.slots, |slot| {
        slots::slot_matches_order(slot, client_order_id, None)
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
    let Some(slots) = slots::clear_matching_slots(&previous_state.slots, |slot| {
        slots::slot_matches_order(slot, "", Some(order_id))
    }) else {
        return previous_state.clone();
    };
    let cleared_slots = previous_state
        .slots
        .iter()
        .filter(|slot| slots::slot_matches_order(slot, "", Some(order_id)))
        .cloned()
        .collect::<Vec<_>>();
    let mut state = previous_state.clone();
    state.slots = slots;
    remember_terminal_orders_from_slots(&mut state, &cleared_slots);
    state
}

pub fn clear_all_working_orders(previous_state: &ExecutorState) -> ExecutorState {
    let Some(slots) = slots::clear_matching_slots(&previous_state.slots, |slot| {
        slot.state == SlotState::Working
    }) else {
        return previous_state.clone();
    };
    let cleared_slots = previous_state
        .slots
        .iter()
        .filter(|slot| slot.state == SlotState::Working)
        .cloned()
        .collect::<Vec<_>>();
    let mut state = previous_state.clone();
    state.slots = slots;
    remember_terminal_orders_from_slots(&mut state, &cleared_slots);
    state
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono::Utc;
    use poise_core::types::{Exposure, Side};

    use super::*;
    use crate::executor::{OrderRole, OrderSlot};
    use crate::runtime::{ExecutionStats, SlotState};

    fn working_state() -> ExecutorState {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        ExecutorState {
            mode: crate::executor::ExecutionMode::Passive,
            inventory_gap: Exposure(4.0),
            gap_started_at: Some(now),
            last_reprice_at: Some(now),
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
            recent_terminal_orders: Vec::new(),
            last_execution_reason: None,
            recovery_anomaly: None,
            stats: ExecutionStats::new(now),
        }
    }

    #[test]
    fn keeps_working_update_without_matching_slot_is_unabsorbed() {
        let previous_state = working_state();

        let applied = apply_order_observation_with_result(
            &previous_state,
            &OrderObservation {
                order_id: "unknown-order".into(),
                client_order_id: "unknown-client".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::New,
            },
        );

        assert_eq!(applied.state, previous_state);
        assert_eq!(applied.absorb_result, OrderUpdateAbsorbResult::Unabsorbed);
    }

    #[test]
    fn identical_working_update_is_duplicate_replay() {
        let previous_state = working_state();

        let applied = apply_order_observation_with_result(
            &previous_state,
            &OrderObservation {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::New,
            },
        );

        assert_eq!(applied.state, previous_state);
        assert_eq!(
            applied.absorb_result,
            OrderUpdateAbsorbResult::DuplicateReplay
        );
    }

    #[test]
    fn repeated_terminal_update_is_duplicate_replay_after_slot_was_already_cleared() {
        let previous_state = working_state();

        let cleared = apply_order_observation_with_result(
            &previous_state,
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
        assert_eq!(cleared.absorb_result, OrderUpdateAbsorbResult::Applied);

        let replay = apply_order_observation_with_result(
            &cleared.state,
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

        assert_eq!(replay.state, cleared.state);
        assert_eq!(
            replay.absorb_result,
            OrderUpdateAbsorbResult::DuplicateReplay
        );
    }

    #[test]
    fn terminal_update_after_cancel_success_clear_is_duplicate_replay() {
        let previous_state = working_state();
        let cleared = clear_working_order_by_order_id(&previous_state, "order-1");

        let replay = apply_order_observation_with_result(
            &cleared,
            &OrderObservation {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::Canceled,
            },
        );

        assert_eq!(replay.state, cleared);
        assert_eq!(
            replay.absorb_result,
            OrderUpdateAbsorbResult::DuplicateReplay
        );
    }

    #[test]
    fn unknown_terminal_update_on_empty_slots_remains_unabsorbed() {
        let previous_state =
            ExecutorState::empty(Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap());

        let applied = apply_order_observation_with_result(
            &previous_state,
            &OrderObservation {
                order_id: "unknown-order".into(),
                client_order_id: "unknown-client".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::Filled,
            },
        );

        assert_eq!(applied.state, previous_state);
        assert_eq!(applied.absorb_result, OrderUpdateAbsorbResult::Unabsorbed);
    }

    #[test]
    fn clearing_submit_pending_does_not_mark_other_working_orders_as_terminal() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let previous_state = ExecutorState {
            mode: crate::executor::ExecutionMode::Passive,
            inventory_gap: Exposure(1.0),
            gap_started_at: Some(now),
            last_reprice_at: Some(now),
            slots: vec![
                ExecutionSlot {
                    slot: OrderSlot::new("inventory_core"),
                    state: SlotState::SubmitPending,
                    working_order: Some(WorkingOrder {
                        order_id: None,
                        client_order_id: "submit-client".into(),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: 5.0,
                        target_exposure: Exposure(2.0),
                        status: OrderStatus::Submitting,
                        role: OrderRole::IncreaseInventory,
                    }),
                },
                ExecutionSlot {
                    slot: OrderSlot::new("other_working"),
                    state: SlotState::Working,
                    working_order: Some(WorkingOrder {
                        order_id: Some("other-order".into()),
                        client_order_id: "other-client".into(),
                        side: Side::Sell,
                        price: 101.0,
                        quantity: 3.0,
                        target_exposure: Exposure(-1.0),
                        status: OrderStatus::New,
                        role: OrderRole::DecreaseInventory,
                    }),
                },
            ],
            recent_terminal_orders: Vec::new(),
            last_execution_reason: None,
            recovery_anomaly: None,
            stats: ExecutionStats::new(now),
        };

        let cleared = clear_pending_submit(&previous_state, "submit-client");
        assert!(cleared.recent_terminal_orders.is_empty());

        let late_terminal = apply_order_observation_with_result(
            &cleared,
            &OrderObservation {
                order_id: "other-order".into(),
                client_order_id: "other-client".into(),
                side: Side::Sell,
                price: 101.0,
                quantity: 3.0,
                realized_pnl: 0.0,
                status: OrderStatus::Filled,
            },
        );

        assert_eq!(
            late_terminal.absorb_result,
            OrderUpdateAbsorbResult::Applied
        );
    }
}

pub(super) fn submit_pending_slot(
    desired_order: &DesiredOrder,
    request: &OrderRequest,
) -> ExecutionSlot {
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

pub(super) fn target_exposure_reached(
    current_exposure: &poise_core::types::Exposure,
    target_exposure: &poise_core::types::Exposure,
) -> bool {
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
