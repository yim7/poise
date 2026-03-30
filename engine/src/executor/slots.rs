use grid_core::types::{Exposure, Side};

use crate::observation::OrderObservation;
use crate::runtime::{ExecutionSlot, ExecutorState, SlotState, WorkingOrder};

use super::{INVENTORY_CORE_SLOT, OrderRole, OrderSlot};

pub(super) fn split_inventory_core_slot(
    executor_state: Option<&ExecutorState>,
) -> (ExecutionSlot, Vec<ExecutionSlot>) {
    let Some(executor_state) = executor_state else {
        return (empty_inventory_core_slot(), Vec::new());
    };

    split_inventory_core_slot_from_slots(&executor_state.slots).0
}

pub(super) fn split_inventory_core_slot_from_slots(
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

pub(super) fn with_inventory_core_slot(
    mut sibling_slots: Vec<ExecutionSlot>,
    inventory_core_slot: ExecutionSlot,
) -> Vec<ExecutionSlot> {
    let mut slots = Vec::with_capacity(sibling_slots.len() + 1);
    slots.push(inventory_core_slot);
    slots.append(&mut sibling_slots);
    slots
}

pub(super) fn replace_first_matching_slot<F>(
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

pub(super) fn clear_matching_slots<F>(
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

pub(super) fn slot_matches_order(
    slot: &ExecutionSlot,
    client_order_id: &str,
    order_id: Option<&str>,
) -> bool {
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

pub(super) fn rebuild_slot_from_live_order(
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

pub(super) fn empty_inventory_core_slot() -> ExecutionSlot {
    empty_slot(&OrderSlot::new(INVENTORY_CORE_SLOT))
}

pub(super) fn empty_slot(slot: &OrderSlot) -> ExecutionSlot {
    ExecutionSlot {
        slot: slot.clone(),
        state: SlotState::Empty,
        working_order: None,
    }
}

pub(super) fn role_for_side(side: Side) -> OrderRole {
    match side {
        Side::Buy => OrderRole::IncreaseInventory,
        Side::Sell => OrderRole::DecreaseInventory,
    }
}
