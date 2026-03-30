use crate::observation::OrderObservation;
use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};
use crate::runtime::{ExecutionSlot, ExecutorState, SlotState, WorkingOrder};

use super::{DesiredOrder, INVENTORY_CORE_SLOT, OrderSlot, slots};

#[derive(Debug, Clone, PartialEq)]
pub enum SubmitReceiptResolution {
    Recorded { state: ExecutorState },
    Unmatched,
}

pub fn record_submit_request(
    previous_state: &ExecutorState,
    request: &OrderRequest,
    target_exposure: grid_core::types::Exposure,
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
                role: slots::role_for_side(request.side),
            }),
        },
    );
    state
}

pub fn record_submit_receipt(
    previous_state: &ExecutorState,
    request: &OrderRequest,
    target_exposure: grid_core::types::Exposure,
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
    if observation.status.keeps_working_order() {
        let Some(slot) = previous_state.slots.iter().find(|slot| {
            slots::slot_matches_order(
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
        return state;
    }

    if observation.status.clears_working_order() {
        let Some(slots) = slots::clear_matching_slots(&previous_state.slots, |slot| {
            slots::slot_matches_order(
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
    let mut state = previous_state.clone();
    state.slots = slots;
    state
}

pub fn clear_all_working_orders(previous_state: &ExecutorState) -> ExecutorState {
    let Some(slots) = slots::clear_matching_slots(&previous_state.slots, |slot| {
        slot.state == SlotState::Working
    }) else {
        return previous_state.clone();
    };
    let mut state = previous_state.clone();
    state.slots = slots;
    state
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
    current_exposure: &grid_core::types::Exposure,
    target_exposure: &grid_core::types::Exposure,
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
