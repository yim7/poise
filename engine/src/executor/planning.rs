use chrono::{DateTime, Utc};
use poise_core::events::ReplacementGateReason;
use poise_core::strategy::TrackConfig;
use poise_core::types::{ExchangeRules, Exposure, Side};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::execution_plan::ExecutionAction;
use crate::execution_plan::{is_meetable_minimum, round_to_step};
use crate::ports::{ExecutionQuote, OrderRequest};
use crate::price_gate::{PriceExecutionGate, SubmitPurpose, allows_submit};
use crate::runtime::ExecutorState;
use crate::track::Instrument;

use super::binding::{
    BindingOperationAllocation, BindingProposal, BindingStatus, LiveOrderBinding,
};
use super::boundary::{
    BoundaryDirection, BoundaryOperation, discretize_boundaries, profile_revision_for_config,
};
use super::ledger::BoundaryLedgerView;
use super::policy::{CoverageReservation, PolicyKind, select_catch_up_operations};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderRole {
    IncreaseInventory,
    DecreaseInventory,
}

pub struct ExecutorInput<'a> {
    pub submit_intent: SubmitIntentInput<'a>,
    pub executor_state: Option<&'a ExecutorState>,
}

#[derive(Debug, Clone)]
pub struct SubmitIntentInput<'a> {
    pub instrument: &'a Instrument,
    pub config: &'a TrackConfig,
    pub exchange_rules: &'a ExchangeRules,
    pub base_qty_per_unit: f64,
    pub min_rebalance_units: f64,
    pub current_exposure: Exposure,
    pub desired_exposure: Exposure,
    pub execution_quote: Option<ExecutionQuote>,
    pub price_execution_gate: PriceExecutionGate,
    pub submit_purpose: SubmitPurpose,
    pub observed_at: DateTime<Utc>,
}

pub struct ExecutorPlan {
    pub state: ExecutorState,
    #[allow(dead_code)]
    pub desired_bindings: Vec<BindingProposal>,
    pub effects: Vec<ExecutionAction>,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSubmitHint {
    pub request: OrderRequest,
    pub desired_exposure: Exposure,
    pub submit_purpose: SubmitPurpose,
}

static CLIENT_ORDER_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl<'a> ExecutorInput<'a> {
    pub fn new(
        submit_intent: SubmitIntentInput<'a>,
        executor_state: Option<&'a ExecutorState>,
    ) -> Self {
        Self {
            submit_intent,
            executor_state,
        }
    }
}

pub fn plan(input: ExecutorInput<'_>) -> ExecutorPlan {
    let ExecutorInput {
        submit_intent,
        executor_state,
    } = input;
    let mut state = executor_state
        .cloned()
        .unwrap_or_else(|| ExecutorState::empty(submit_intent.observed_at))
        .ensure_revision(submit_intent.config, submit_intent.current_exposure.clone());

    if state.recovery_anomaly.is_some()
        || !allows_submit(
            submit_intent.price_execution_gate,
            submit_intent.submit_purpose,
        )
    {
        return noop_plan(state);
    }

    let inventory_gap = submit_intent
        .current_exposure
        .delta(&submit_intent.desired_exposure);
    if inventory_gap.0.abs() < submit_intent.min_rebalance_units {
        return noop_plan(state);
    }

    let Some(direction) = direction_for_gap(&inventory_gap) else {
        return noop_plan(state);
    };
    let Some(price) = execution_price(direction, submit_intent.execution_quote) else {
        return noop_plan(state);
    };

    let boundaries = discretize_boundaries(
        submit_intent.config,
        profile_revision_for_config(submit_intent.config),
    );
    let view = BoundaryLedgerView::from_boundaries(
        &boundaries,
        &state.ledger_state,
        submit_intent.desired_exposure.clone(),
        exposure_epsilon(&submit_intent),
    );
    let coverage = coverage_from_bindings(&state.bindings);
    let selected = select_catch_up_operations(&view, &coverage, exposure_epsilon(&submit_intent))
        .into_iter()
        .filter(|operation| operation.direction == direction)
        .collect::<Vec<_>>();
    let allocations = allocate_operations(&view, selected, inventory_gap.0.abs());
    if allocations.is_empty() {
        return noop_plan(state);
    }

    let exposure_qty = allocations
        .iter()
        .map(|allocation| allocation.exposure_qty)
        .sum::<f64>();
    let quantity = round_to_step(
        exposure_qty * submit_intent.base_qty_per_unit,
        submit_intent.exchange_rules.quantity_step,
    );
    if quantity <= f64::EPSILON
        || !is_meetable_minimum(price, quantity, submit_intent.exchange_rules)
    {
        return noop_plan(state);
    }

    let side = side_for_direction(direction);
    let role = role_for_target_change(
        &submit_intent.current_exposure,
        &submit_intent.desired_exposure,
    );
    let request = OrderRequest {
        instrument: submit_intent.instrument.clone(),
        side,
        price,
        quantity,
        client_order_id: next_client_order_id(),
        reduce_only: role == OrderRole::DecreaseInventory,
    };
    let operations = allocations
        .iter()
        .map(|allocation| allocation.operation.clone())
        .collect::<Vec<_>>();
    let proposal = BindingProposal {
        policy: PolicyKind::CatchUp,
        operations,
    };
    let proposal_key = proposal.proposal_key();
    let binding = LiveOrderBinding {
        binding_id: request.client_order_id.clone(),
        proposal_key,
        allocations,
        request: request.clone(),
        desired_exposure: submit_intent.desired_exposure.clone(),
        submit_purpose: submit_intent.submit_purpose,
        order_id: None,
        status: BindingStatus::SubmitPending,
    };

    state.bindings.push(binding);
    ExecutorPlan {
        state,
        desired_bindings: vec![proposal],
        effects: vec![ExecutionAction::SubmitOrder {
            request,
            desired_exposure: submit_intent.desired_exposure,
            submit_purpose: submit_intent.submit_purpose,
        }],
        replacement_gate_reason: None,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn current_submit_hint(input: SubmitIntentInput<'_>) -> Option<PendingSubmitHint> {
    let plan = plan(ExecutorInput::new(input, None));
    plan.effects.into_iter().find_map(|effect| match effect {
        ExecutionAction::SubmitOrder {
            request,
            desired_exposure,
            submit_purpose,
        } => Some(PendingSubmitHint {
            request,
            desired_exposure,
            submit_purpose,
        }),
        _ => None,
    })
}

pub fn refresh_state(
    previous_state: &ExecutorState,
    config: &TrackConfig,
    current_exposure: &Exposure,
    _desired_exposure: &Exposure,
    _min_rebalance_units: f64,
    _observed_at: DateTime<Utc>,
) -> ExecutorState {
    previous_state.ensure_revision(config, current_exposure.clone())
}

fn noop_plan(state: ExecutorState) -> ExecutorPlan {
    ExecutorPlan {
        state,
        desired_bindings: Vec::new(),
        effects: vec![ExecutionAction::NoOp],
        replacement_gate_reason: None,
    }
}

fn coverage_from_bindings(bindings: &[LiveOrderBinding]) -> CoverageReservation {
    let mut coverage = CoverageReservation::default();
    for binding in bindings
        .iter()
        .filter(|binding| binding.status != BindingStatus::Terminal)
    {
        for allocation in &binding.allocations {
            coverage.reserve(allocation.operation.clone());
        }
    }
    coverage
}

fn allocate_operations(
    view: &BoundaryLedgerView,
    operations: Vec<BoundaryOperation>,
    max_exposure_qty: f64,
) -> Vec<BindingOperationAllocation> {
    let mut remaining_budget = max_exposure_qty;
    let mut allocations = Vec::new();
    for operation in operations {
        if remaining_budget <= f64::EPSILON {
            break;
        }
        let Some(operation_view) = view
            .operations
            .iter()
            .find(|candidate| candidate.operation == operation)
        else {
            continue;
        };
        let exposure_qty = operation_view.remaining.min(remaining_budget);
        if exposure_qty <= f64::EPSILON {
            continue;
        }
        remaining_budget -= exposure_qty;
        allocations.push(BindingOperationAllocation {
            operation,
            exposure_qty,
        });
    }
    allocations
}

fn direction_for_gap(gap: &Exposure) -> Option<BoundaryDirection> {
    if gap.0 > f64::EPSILON {
        Some(BoundaryDirection::Up)
    } else if gap.0 < -f64::EPSILON {
        Some(BoundaryDirection::Down)
    } else {
        None
    }
}

fn side_for_direction(direction: BoundaryDirection) -> Side {
    match direction {
        BoundaryDirection::Up => Side::Buy,
        BoundaryDirection::Down => Side::Sell,
    }
}

fn execution_price(direction: BoundaryDirection, quote: Option<ExecutionQuote>) -> Option<f64> {
    let quote = quote?;
    Some(match direction {
        BoundaryDirection::Up => quote.best_ask,
        BoundaryDirection::Down => quote.best_bid,
    })
}

fn exposure_epsilon(input: &SubmitIntentInput<'_>) -> f64 {
    let quantity_step_as_exposure = if input.base_qty_per_unit <= f64::EPSILON {
        0.0
    } else {
        input.exchange_rules.quantity_step / input.base_qty_per_unit
    };
    (input.min_rebalance_units * 0.01).max(quantity_step_as_exposure)
}

fn role_for_target_change(current_exposure: &Exposure, desired_exposure: &Exposure) -> OrderRole {
    if desired_exposure.0.abs() + f64::EPSILON < current_exposure.0.abs() {
        OrderRole::DecreaseInventory
    } else {
        OrderRole::IncreaseInventory
    }
}

fn next_client_order_id() -> String {
    let sequence = CLIENT_ORDER_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    format!("boundary-catch-up-{sequence}")
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::ExchangeRules;

    use super::*;
    use crate::ports::ExecutionQuote;
    use crate::price_gate::PriceExecutionGate;
    use crate::track::{Instrument, Venue};

    fn config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        }
    }

    fn rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.01,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    fn input<'a>(
        config: &'a TrackConfig,
        rules: &'a ExchangeRules,
        current_exposure: Exposure,
        desired_exposure: Exposure,
    ) -> ExecutorInput<'a> {
        let instrument = Box::leak(Box::new(Instrument::new(Venue::Binance, "BTCUSDT")));
        ExecutorInput::new(
            SubmitIntentInput {
                instrument,
                config,
                exchange_rules: rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure,
                desired_exposure,
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        )
    }

    #[test]
    fn catch_up_policy_submits_buy_for_due_up_operation_when_uncovered() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0)));

        let [ExecutionAction::SubmitOrder { request, .. }] = plan.effects.as_slice() else {
            panic!("expected one submit order");
        };
        assert_eq!(request.side, Side::Buy);
        assert_eq!(request.price, 100.1);
        assert!((request.quantity - 2.0).abs() < 1e-9);
        assert_eq!(plan.state.bindings.len(), 1);
        assert_eq!(plan.state.bindings[0].allocations.len(), 2);
    }

    #[test]
    fn planning_no_longer_depends_on_active_round_or_slots() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0)));
        let state_json = serde_json::to_value(&plan.state).unwrap();

        assert!(state_json.get("active_round").is_none());
        assert!(state_json.get("slots").is_none());
        assert!(state_json.get("ledger_state").is_some());
        assert!(state_json.get("bindings").is_some());
    }
}
