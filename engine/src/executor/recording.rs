use crate::observation::OrderObservation;
use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};
use crate::runtime::{ExecutorState, RecentTerminalOrder};
use poise_core::types::Exposure;

use super::binding::{BindingStatus, LiveOrderBinding};
use super::ledger::{BoundaryProgress, BoundaryProgressEntry};

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

pub struct OrderObservationApplication {
    pub state: ExecutorState,
    pub absorb_result: OrderUpdateAbsorbResult,
}

pub fn record_submit_request(
    previous_state: &ExecutorState,
    _request: &OrderRequest,
    _desired_exposure: Exposure,
) -> ExecutorState {
    previous_state.clone()
}

pub fn record_submit_receipt(
    previous_state: &ExecutorState,
    request: &OrderRequest,
    _desired_exposure: Exposure,
    receipt: &OrderReceipt,
) -> SubmitReceiptResolution {
    let Some(index) = previous_state.bindings.iter().position(|binding| {
        binding.request.client_order_id == request.client_order_id
            && binding
                .order_id
                .as_deref()
                .is_none_or(|order_id| order_id == receipt.order_id)
    }) else {
        return SubmitReceiptResolution::Unmatched;
    };

    let mut state = previous_state.clone();
    state.bindings[index].order_id = Some(receipt.order_id.clone());
    state.bindings[index].status = if receipt.status.keeps_working_order() {
        BindingStatus::Working
    } else {
        BindingStatus::Terminal
    };
    SubmitReceiptResolution::Recorded { state }
}

pub fn record_submit_failure(
    previous_state: &ExecutorState,
    client_order_id: &str,
) -> ExecutorState {
    clear_binding_by_client_order_id(previous_state, client_order_id)
}

#[allow(dead_code)]
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
    let Some(index) = previous_state
        .bindings
        .iter()
        .position(|binding| binding_matches_observation(binding, observation))
    else {
        let absorb_result = if is_recent_terminal_order(previous_state, observation) {
            OrderUpdateAbsorbResult::DuplicateReplay
        } else {
            OrderUpdateAbsorbResult::Unabsorbed
        };
        return OrderObservationApplication {
            state: previous_state.clone(),
            absorb_result,
        };
    };

    let mut state = previous_state.clone();
    state.bindings[index].order_id = Some(observation.order_id.clone());
    state.bindings[index].request.price = observation.price;
    state.bindings[index].request.quantity = observation.quantity;

    if observation.status.keeps_working_order() {
        state.bindings[index].status = BindingStatus::Working;
    } else if observation.status.clears_working_order() {
        if observation.status == OrderStatus::Filled {
            let binding = state.bindings[index].clone();
            apply_completed_binding_progress(&mut state, &binding);
        }
        state.bindings[index].status = BindingStatus::Terminal;
        remember_terminal_order(
            &mut state,
            &observation.client_order_id,
            &observation.order_id,
        );
    }

    let absorb_result = if state == *previous_state {
        OrderUpdateAbsorbResult::DuplicateReplay
    } else {
        OrderUpdateAbsorbResult::Applied
    };
    OrderObservationApplication {
        state,
        absorb_result,
    }
}

pub fn clear_working_order_by_order_id(
    previous_state: &ExecutorState,
    order_id: &str,
) -> ExecutorState {
    let mut state = previous_state.clone();
    for binding in &mut state.bindings {
        if binding.order_id.as_deref() == Some(order_id) {
            binding.status = BindingStatus::Terminal;
        }
    }
    state
}

pub fn clear_all_working_orders(previous_state: &ExecutorState) -> ExecutorState {
    let mut state = previous_state.clone();
    for binding in &mut state.bindings {
        if binding.status == BindingStatus::Working {
            binding.status = BindingStatus::Terminal;
        }
    }
    state
}

fn clear_binding_by_client_order_id(
    previous_state: &ExecutorState,
    client_order_id: &str,
) -> ExecutorState {
    let mut state = previous_state.clone();
    for binding in &mut state.bindings {
        if binding.request.client_order_id == client_order_id {
            binding.status = BindingStatus::Terminal;
        }
    }
    state
}

fn binding_matches_observation(binding: &LiveOrderBinding, observation: &OrderObservation) -> bool {
    binding.request.client_order_id == observation.client_order_id
        || binding.order_id.as_deref() == Some(observation.order_id.as_str())
}

fn apply_completed_binding_progress(state: &mut ExecutorState, binding: &LiveOrderBinding) {
    for allocation in &binding.allocations {
        let Some(entry) = state
            .ledger_state
            .progress
            .iter_mut()
            .find(|entry| entry.boundary_id == allocation.operation.boundary_id)
        else {
            state.ledger_state.progress.push(BoundaryProgressEntry {
                boundary_id: allocation.operation.boundary_id.clone(),
                progress: progress_delta_for_allocation(allocation),
            });
            continue;
        };
        let delta = progress_delta_for_allocation(allocation);
        entry.progress.cumulative_up += delta.cumulative_up;
        entry.progress.cumulative_down += delta.cumulative_down;
    }
}

fn progress_delta_for_allocation(
    allocation: &super::binding::BindingOperationAllocation,
) -> BoundaryProgress {
    match allocation.operation.direction {
        super::boundary::BoundaryDirection::Up => BoundaryProgress {
            cumulative_up: allocation.exposure_qty,
            cumulative_down: 0.0,
        },
        super::boundary::BoundaryDirection::Down => BoundaryProgress {
            cumulative_up: 0.0,
            cumulative_down: allocation.exposure_qty,
        },
    }
}

fn remember_terminal_order(state: &mut ExecutorState, client_order_id: &str, order_id: &str) {
    if state
        .recent_terminal_orders
        .iter()
        .any(|order| order.client_order_id == client_order_id && order.order_id == order_id)
    {
        return;
    }
    state.recent_terminal_orders.push(RecentTerminalOrder {
        client_order_id: client_order_id.to_string(),
        order_id: order_id.to_string(),
    });
    const MAX_RECENT_TERMINAL_ORDERS: usize = 32;
    if state.recent_terminal_orders.len() > MAX_RECENT_TERMINAL_ORDERS {
        let excess = state.recent_terminal_orders.len() - MAX_RECENT_TERMINAL_ORDERS;
        state.recent_terminal_orders.drain(0..excess);
    }
}

fn is_recent_terminal_order(state: &ExecutorState, observation: &OrderObservation) -> bool {
    state.recent_terminal_orders.iter().any(|order| {
        order.client_order_id == observation.client_order_id
            && order.order_id == observation.order_id
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure};

    use super::*;
    use crate::executor::planning::{ExecutorInput, SubmitIntentInput, plan};
    use crate::ports::ExecutionQuote;
    use crate::price_gate::{PriceExecutionGate, SubmitPurpose};
    use crate::track::{Instrument, Venue};

    #[test]
    fn recording_applies_fill_to_binding_then_updates_boundary_progress() {
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        };
        let rules = ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.01,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        let planned = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: &instrument,
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.0),
                desired_exposure: Exposure(1.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        ));
        let binding = &planned.state.bindings[0];
        let observation = OrderObservation {
            order_id: "order-1".to_string(),
            client_order_id: binding.request.client_order_id.clone(),
            side: binding.request.side,
            price: binding.request.price,
            quantity: binding.request.quantity,
            realized_pnl: 0.0,
            status: OrderStatus::Filled,
        };

        let applied = apply_order_observation_with_result(&planned.state, &observation);

        assert_eq!(applied.absorb_result, OrderUpdateAbsorbResult::Applied);
        assert_eq!(applied.state.ledger_state.progress.len(), 1);
        assert!(
            (applied.state.ledger_state.progress[0]
                .progress
                .cumulative_up
                - 1.0)
                .abs()
                < 1e-9
        );
    }
}
