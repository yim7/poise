use crate::execution_plan::TrackEffect;
use crate::observation::OrderObservation;
use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};
use crate::runtime::{ExecutorState, RecentTerminalOrder};
use poise_core::types::Exposure;

use super::binding::{BindingStatus, LiveOrderBinding, SubmitRecoveryToken};
use super::ledger::BoundaryProgress;

#[derive(Debug, Clone, PartialEq)]
pub enum SubmitReceiptResolution {
    Recorded {
        state: ExecutorState,
        effects: Vec<TrackEffect>,
    },
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
    pub effects: Vec<TrackEffect>,
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
        if is_recent_terminal_receipt(previous_state, request, receipt) {
            return SubmitReceiptResolution::Recorded {
                state: previous_state.clone(),
                effects: Vec::new(),
            };
        }
        return SubmitReceiptResolution::Unmatched;
    };

    let previous_status = previous_state.bindings[index].status;
    let mut state = previous_state.clone();
    state.bindings[index].order_id = Some(receipt.order_id.clone());
    absorb_binding_fill_progress(&mut state, index, receipt.filled_qty, receipt.status);
    let (status, effects) = status_after_order_fact(
        &state.bindings[index],
        previous_status,
        &receipt.order_id,
        receipt.status,
    );
    state.bindings[index].status = status;
    if receipt.status.clears_working_order() {
        remember_terminal_order(&mut state, &request.client_order_id, &receipt.order_id);
    }
    SubmitReceiptResolution::Recorded { state, effects }
}

pub fn record_submit_failure(
    previous_state: &ExecutorState,
    client_order_id: &str,
) -> ExecutorState {
    clear_binding_by_client_order_id(previous_state, client_order_id)
}

pub fn record_submit_failure_by_recovery_token(
    previous_state: &ExecutorState,
    recovery_token: &SubmitRecoveryToken,
) -> ExecutorState {
    let Some(target) = recovery_token.decode() else {
        return previous_state.clone();
    };
    let mut state = previous_state.clone();
    for binding in &mut state.bindings {
        if binding.binding_id == target.binding_id {
            binding.status = BindingStatus::Terminal;
        }
    }
    state
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
            effects: Vec::new(),
        };
    };

    apply_order_observation_to_binding_with_result(previous_state, index, observation)
}

pub(super) fn apply_order_observation_to_binding_with_result(
    previous_state: &ExecutorState,
    index: usize,
    observation: &OrderObservation,
) -> OrderObservationApplication {
    let previous_status = previous_state.bindings[index].status;
    let mut state = previous_state.clone();
    state.bindings[index].order_id = Some(observation.order_id.clone());
    state.bindings[index].request.price = observation.price;
    state.bindings[index].request.quantity = observation.quantity;
    absorb_binding_fill_progress(
        &mut state,
        index,
        observation.filled_qty,
        observation.status,
    );

    let (status, effects) = status_after_order_fact(
        &state.bindings[index],
        previous_status,
        &observation.order_id,
        observation.status,
    );
    state.bindings[index].status = status;
    if observation.status.clears_working_order() {
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
        effects,
    }
}

fn status_after_order_fact(
    binding: &LiveOrderBinding,
    previous_status: BindingStatus,
    order_id: &str,
    order_status: OrderStatus,
) -> (BindingStatus, Vec<TrackEffect>) {
    if previous_status == BindingStatus::Terminal {
        return (BindingStatus::Terminal, Vec::new());
    }
    if order_status.clears_working_order() {
        return (BindingStatus::Terminal, Vec::new());
    }
    if !order_status.keeps_working_order() {
        return (previous_status, Vec::new());
    }
    match previous_status {
        BindingStatus::CancelAfterSubmit => (
            BindingStatus::CancelPending,
            vec![TrackEffect::CancelOrder {
                instrument: binding.request.instrument.clone(),
                order_id: order_id.to_string(),
            }],
        ),
        BindingStatus::CancelPending => (BindingStatus::CancelPending, Vec::new()),
        _ => (BindingStatus::Working, Vec::new()),
    }
}

pub fn record_cancel_order_receipt(
    previous_state: &ExecutorState,
    order_id: &str,
    receipt: &OrderReceipt,
) -> ExecutorState {
    let mut state = previous_state.clone();
    let Some(index) = state.bindings.iter().position(|binding| {
        binding.order_id.as_deref() == Some(order_id)
            || binding.order_id.as_deref() == Some(receipt.order_id.as_str())
    }) else {
        return state;
    };

    state.bindings[index].order_id = Some(receipt.order_id.clone());
    absorb_binding_fill_progress(&mut state, index, receipt.filled_qty, receipt.status);
    if receipt.status.keeps_working_order() {
        state.bindings[index].status = BindingStatus::Working;
    } else if receipt.status.clears_working_order() {
        state.bindings[index].status = BindingStatus::Terminal;
        let client_order_id = state.bindings[index].request.client_order_id.clone();
        remember_terminal_order(&mut state, &client_order_id, &receipt.order_id);
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

fn absorb_binding_fill_progress(
    state: &mut ExecutorState,
    binding_index: usize,
    reported_filled_qty: f64,
    status: OrderStatus,
) {
    let binding = state.bindings[binding_index].clone();
    let total_allocated_exposure_qty = binding.total_allocated_exposure_qty();
    if total_allocated_exposure_qty <= f64::EPSILON || binding.request.quantity <= f64::EPSILON {
        return;
    }

    let normalized_filled_qty =
        status.effective_cumulative_filled_qty(reported_filled_qty, binding.request.quantity);
    let target_absorbed_exposure_qty =
        normalized_filled_qty / binding.request.quantity * total_allocated_exposure_qty;
    let delta_exposure_qty =
        target_absorbed_exposure_qty - state.bindings[binding_index].absorbed_exposure_qty;
    if delta_exposure_qty <= f64::EPSILON {
        return;
    }

    apply_binding_progress_delta(
        state,
        &binding.allocations,
        state.bindings[binding_index].absorbed_exposure_qty,
        delta_exposure_qty,
    );
    state.bindings[binding_index].absorbed_exposure_qty = target_absorbed_exposure_qty;
}

fn apply_binding_progress_delta(
    state: &mut ExecutorState,
    allocations: &[super::binding::BindingOperationAllocation],
    already_absorbed_exposure_qty: f64,
    delta_exposure_qty: f64,
) {
    let mut cursor = 0.0;
    let mut remaining_delta = delta_exposure_qty;
    let mut last_allocation = None;
    for allocation in allocations {
        last_allocation = Some(allocation);
        let allocation_start = cursor;
        let allocation_end = allocation_start + allocation.exposure_qty;
        cursor = allocation_end;
        let already_absorbed_in_allocation =
            (already_absorbed_exposure_qty - allocation_start).clamp(0.0, allocation.exposure_qty);
        let remaining_allocation_qty =
            (allocation.exposure_qty - already_absorbed_in_allocation).max(0.0);
        let apply_qty = remaining_delta.min(remaining_allocation_qty);
        if apply_qty > f64::EPSILON {
            apply_progress_delta_for_allocation(state, allocation, apply_qty);
            remaining_delta -= apply_qty;
        }
        if remaining_delta <= f64::EPSILON {
            return;
        }
    }

    if let Some(allocation) = last_allocation {
        apply_progress_delta_for_allocation(state, allocation, remaining_delta);
    }
}

fn apply_progress_delta_for_allocation(
    state: &mut ExecutorState,
    allocation: &super::binding::BindingOperationAllocation,
    exposure_qty: f64,
) {
    let delta = progress_delta_for_allocation(allocation, exposure_qty);
    let progress = state
        .ledger_state
        .progress
        .entry(allocation.operation.boundary_id.clone())
        .or_default();
    progress.cumulative_up += delta.cumulative_up;
    progress.cumulative_down += delta.cumulative_down;
}

fn progress_delta_for_allocation(
    allocation: &super::binding::BindingOperationAllocation,
    exposure_qty: f64,
) -> BoundaryProgress {
    match allocation.operation.direction {
        super::boundary::BoundaryDirection::Up => BoundaryProgress {
            cumulative_up: exposure_qty,
            cumulative_down: 0.0,
        },
        super::boundary::BoundaryDirection::Down => BoundaryProgress {
            cumulative_up: 0.0,
            cumulative_down: exposure_qty,
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

fn is_recent_terminal_receipt(
    state: &ExecutorState,
    request: &OrderRequest,
    receipt: &OrderReceipt,
) -> bool {
    state.recent_terminal_orders.iter().any(|order| {
        order.client_order_id == request.client_order_id && order.order_id == receipt.order_id
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure};

    use super::*;
    use crate::execution_plan::TrackEffect;
    use crate::executor::ledger::BoundaryLedgerState;
    use crate::executor::{ExecutorInput, PolicyContext, SubmitIntentInput, plan};
    use crate::ports::ExecutionQuote;
    use crate::price_gate::{PriceExecutionGate, SubmitPurpose};
    use poise_core::track::{Instrument, Venue};

    fn only_progress(ledger_state: &BoundaryLedgerState) -> &BoundaryProgress {
        ledger_state
            .progress
            .values()
            .next()
            .expect("test ledger should contain one progress entry")
    }

    fn pending_submit_state() -> ExecutorState {
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
            quantity_step: 0.01,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: &instrument,
                config: &config,
                exchange_rules: &rules,
                native_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(1.0),
                execution_target: Exposure(1.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        ))
        .state
    }

    #[test]
    fn order_update_emits_cancel_once_for_cancel_after_submit_binding() {
        let mut state = pending_submit_state();
        state.bindings[0].status = BindingStatus::CancelAfterSubmit;
        let binding = state.bindings[0].clone();
        let observation = OrderObservation {
            order_id: "protected-order".to_string(),
            client_order_id: binding.request.client_order_id.clone(),
            side: binding.request.side,
            price: binding.request.price,
            quantity: binding.request.quantity,
            filled_qty: 0.0,
            status: OrderStatus::New,
        };

        let applied = apply_order_observation_with_result(&state, &observation);

        assert_eq!(
            applied.state.bindings[0].status,
            BindingStatus::CancelPending
        );
        assert_eq!(
            applied
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    TrackEffect::CancelOrder { order_id, .. } if order_id == "protected-order"
                ))
                .count(),
            1
        );

        let replay = apply_order_observation_with_result(&applied.state, &observation);
        assert_eq!(
            replay.state.bindings[0].status,
            BindingStatus::CancelPending
        );
        assert!(replay.effects.is_empty());

        let SubmitReceiptResolution::Recorded {
            state: receipt_state,
            effects: receipt_effects,
        } = record_submit_receipt(
            &applied.state,
            &binding.request,
            binding.desired_exposure,
            &OrderReceipt {
                order_id: "protected-order".to_string(),
                client_order_id: binding.request.client_order_id.clone(),
                filled_qty: 0.0,
                status: OrderStatus::New,
            },
        )
        else {
            panic!("replayed receipt should still match the binding");
        };
        assert_eq!(
            receipt_state.bindings[0].status,
            BindingStatus::CancelPending
        );
        assert!(receipt_effects.is_empty());
    }

    #[test]
    fn terminal_submit_receipt_does_not_cancel_after_submit() {
        let mut state = pending_submit_state();
        state.bindings[0].status = BindingStatus::CancelAfterSubmit;
        let binding = state.bindings[0].clone();
        let receipt = OrderReceipt {
            order_id: "filled-before-cancel".to_string(),
            client_order_id: binding.request.client_order_id.clone(),
            filled_qty: binding.request.quantity,
            status: OrderStatus::Filled,
        };

        let SubmitReceiptResolution::Recorded {
            state: recorded,
            effects,
        } = record_submit_receipt(&state, &binding.request, binding.desired_exposure, &receipt)
        else {
            panic!("terminal receipt should match the authorized binding");
        };

        assert_eq!(recorded.bindings[0].status, BindingStatus::Terminal);
        assert!(effects.is_empty());
        assert_eq!(recorded.ledger_state.progress.len(), 1);

        let mut pruned = recorded;
        pruned.bindings.clear();
        let stale_working = apply_order_observation_with_result(
            &pruned,
            &OrderObservation {
                order_id: receipt.order_id,
                client_order_id: receipt.client_order_id,
                side: binding.request.side,
                price: binding.request.price,
                quantity: binding.request.quantity,
                filled_qty: 0.0,
                status: OrderStatus::New,
            },
        );
        assert_eq!(
            stale_working.absorb_result,
            OrderUpdateAbsorbResult::DuplicateReplay,
            "a stale working fact must remain recognized after the terminal binding is pruned"
        );
        assert!(stale_working.state.bindings.is_empty());
        assert!(stale_working.effects.is_empty());
    }

    #[test]
    fn definitive_submit_failure_terminalizes_authorized_lifecycle_states() {
        for status in [
            BindingStatus::SubmitAuthorized,
            BindingStatus::CancelAfterSubmit,
        ] {
            let mut state = pending_submit_state();
            state.bindings[0].status = status;
            let client_order_id = state.bindings[0].request.client_order_id.clone();

            let recorded = record_submit_failure(&state, &client_order_id);

            assert_eq!(recorded.bindings[0].status, BindingStatus::Terminal);
        }
    }

    #[test]
    fn stale_working_facts_do_not_revive_terminal_submit_binding() {
        let mut state = pending_submit_state();
        state.bindings[0].status = BindingStatus::SubmitAuthorized;
        let binding = state.bindings[0].clone();
        let terminal = OrderObservation {
            order_id: "terminal-before-submit-ack".to_string(),
            client_order_id: binding.request.client_order_id.clone(),
            side: binding.request.side,
            price: binding.request.price,
            quantity: binding.request.quantity,
            filled_qty: binding.request.quantity,
            status: OrderStatus::Filled,
        };
        let terminal_applied = apply_order_observation_with_result(&state, &terminal);
        assert_eq!(
            terminal_applied.state.bindings[0].status,
            BindingStatus::Terminal
        );

        let stale_working = OrderObservation {
            filled_qty: 0.0,
            status: OrderStatus::New,
            ..terminal.clone()
        };
        let stale_applied =
            apply_order_observation_with_result(&terminal_applied.state, &stale_working);
        assert_eq!(
            stale_applied.state.bindings[0].status,
            BindingStatus::Terminal
        );
        assert!(stale_applied.effects.is_empty());

        let mut pruned_terminal = terminal_applied.state;
        pruned_terminal.bindings.clear();
        let SubmitReceiptResolution::Recorded {
            state: duplicate_state,
            effects,
        } = record_submit_receipt(
            &pruned_terminal,
            &binding.request,
            binding.desired_exposure,
            &OrderReceipt {
                order_id: terminal.order_id,
                client_order_id: binding.request.client_order_id.clone(),
                filled_qty: 0.0,
                status: OrderStatus::New,
            },
        )
        else {
            panic!("late submit ack should match recent terminal identity");
        };
        assert!(duplicate_state.bindings.is_empty());
        assert!(effects.is_empty());
    }

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
            risk_acquisition: Default::default(),
        };
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
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
                native_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(1.0),
                execution_target: Exposure(1.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
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
            filled_qty: binding.request.quantity,
            status: OrderStatus::Filled,
        };

        let applied = apply_order_observation_with_result(&planned.state, &observation);

        assert_eq!(applied.absorb_result, OrderUpdateAbsorbResult::Applied);
        assert_eq!(applied.state.ledger_state.progress.len(), 1);
        assert!((only_progress(&applied.state.ledger_state).cumulative_up - 1.0).abs() < 1e-9);
    }

    #[test]
    fn recording_applies_partial_fill_incrementally_and_deduplicates_replay() {
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
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
                native_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(1.0),
                execution_target: Exposure(1.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        ));
        let binding = &planned.state.bindings[0];
        let partial = OrderObservation {
            order_id: "order-1".to_string(),
            client_order_id: binding.request.client_order_id.clone(),
            side: binding.request.side,
            price: binding.request.price,
            quantity: binding.request.quantity,
            filled_qty: binding.request.quantity * 0.4,
            status: OrderStatus::PartiallyFilled,
        };

        let first = apply_order_observation_with_result(&planned.state, &partial);
        let replay = apply_order_observation_with_result(&first.state, &partial);

        assert_eq!(first.absorb_result, OrderUpdateAbsorbResult::Applied);
        assert_eq!(
            replay.absorb_result,
            OrderUpdateAbsorbResult::DuplicateReplay
        );
        assert_eq!(first.state.bindings[0].status, BindingStatus::Working);
        assert_eq!(first.state.ledger_state.progress.len(), 1);
        assert!((only_progress(&first.state.ledger_state).cumulative_up - 0.4).abs() < 1e-9);
        assert!((only_progress(&replay.state.ledger_state).cumulative_up - 0.4).abs() < 1e-9);
    }

    #[test]
    fn recording_preserves_partial_progress_when_order_cancels() {
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
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
                native_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(1.0),
                execution_target: Exposure(1.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        ));
        let binding = &planned.state.bindings[0];
        let canceled = OrderObservation {
            order_id: "order-1".to_string(),
            client_order_id: binding.request.client_order_id.clone(),
            side: binding.request.side,
            price: binding.request.price,
            quantity: binding.request.quantity,
            filled_qty: binding.request.quantity * 0.25,
            status: OrderStatus::Canceled,
        };

        let applied = apply_order_observation_with_result(&planned.state, &canceled);

        assert_eq!(applied.absorb_result, OrderUpdateAbsorbResult::Applied);
        assert_eq!(applied.state.bindings[0].status, BindingStatus::Terminal);
        assert_eq!(applied.state.ledger_state.progress.len(), 1);
        assert!((only_progress(&applied.state.ledger_state).cumulative_up - 0.25).abs() < 1e-9);
    }

    #[test]
    fn partial_fill_then_cancel_waits_when_remaining_gap_is_below_min_rebalance() {
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
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
                native_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(1.0),
                execution_target: Exposure(1.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        ));
        let binding = &planned.state.bindings[0];
        let canceled = OrderObservation {
            order_id: "order-1".to_string(),
            client_order_id: binding.request.client_order_id.clone(),
            side: binding.request.side,
            price: binding.request.price,
            quantity: binding.request.quantity,
            filled_qty: binding.request.quantity * 0.4,
            status: OrderStatus::Canceled,
        };
        let applied = apply_order_observation_with_result(&planned.state, &canceled);

        let next = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: &instrument,
                config: &config,
                exchange_rules: &rules,
                native_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.4),
                risk_approved_target: Exposure(1.0),
                execution_target: Exposure(1.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&applied.state),
        ));

        assert!(!next.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::SubmitOrder { request, .. }
                if request.client_order_id.starts_with("bc")
        )));
    }

    #[test]
    fn recording_applies_cancel_receipt_fill_to_boundary_progress() {
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
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
                native_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(1.0),
                execution_target: Exposure(1.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        ));
        let mut state = planned.state.clone();
        state.bindings[0].order_id = Some("order-1".to_string());
        state.bindings[0].status = BindingStatus::CancelPending;
        let receipt = OrderReceipt {
            order_id: "order-1".to_string(),
            client_order_id: state.bindings[0].request.client_order_id.clone(),
            filled_qty: state.bindings[0].request.quantity * 0.25,
            status: OrderStatus::Canceled,
        };

        let applied = record_cancel_order_receipt(&state, "order-1", &receipt);

        assert_eq!(applied.bindings[0].status, BindingStatus::Terminal);
        assert_eq!(applied.ledger_state.progress.len(), 1);
        assert!((only_progress(&applied.ledger_state).cumulative_up - 0.25).abs() < 1e-9);
    }

    #[test]
    fn recording_applies_filled_submit_receipt_to_boundary_progress() {
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
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
                native_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(1.0),
                execution_target: Exposure(1.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        ));
        let binding = &planned.state.bindings[0];
        let receipt = OrderReceipt {
            order_id: "order-1".to_string(),
            client_order_id: binding.request.client_order_id.clone(),
            filled_qty: binding.request.quantity,
            status: OrderStatus::Filled,
        };

        let SubmitReceiptResolution::Recorded { state, .. } = record_submit_receipt(
            &planned.state,
            &binding.request,
            binding.desired_exposure.clone(),
            &receipt,
        ) else {
            panic!("submit receipt should match planned binding");
        };

        assert_eq!(state.bindings[0].status, BindingStatus::Terminal);
        assert_eq!(state.ledger_state.progress.len(), 1);
        assert!((only_progress(&state.ledger_state).cumulative_up - 1.0).abs() < 1e-9);
    }
}
