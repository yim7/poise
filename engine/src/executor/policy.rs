use std::collections::BTreeSet;

use poise_core::strategy::TrackConfig;
use poise_core::types::{ExchangeRules, Exposure, Side};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::execution_plan::{is_meetable_minimum, round_to_step};
use crate::executor::binding::{
    BindingOperationAllocation, BindingPolicyState, BindingProposal, BindingStatus,
    LiveOrderBinding, binding_is_active,
};
use crate::executor::boundary::{
    BoundaryBlueprint, BoundaryDirection, BoundaryOperation, trigger_price_for_boundary,
};
use crate::executor::ledger::BoundaryLedgerView;
use crate::ports::{ExecutionQuote, OrderRequest};
use crate::price_gate::SubmitPurpose;
use poise_core::track::Instrument;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    ManualOverride,
    ReduceOnly,
    CatchUp,
    CurveMaker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyContext {
    Normal,
    ManualOverride,
    ReduceOnly,
}

pub(super) struct PolicyPlanningInput<'a> {
    pub view: &'a BoundaryLedgerView,
    pub boundaries: &'a [BoundaryBlueprint],
    pub instrument: &'a Instrument,
    pub config: &'a TrackConfig,
    pub exchange_rules: &'a ExchangeRules,
    pub base_qty_per_unit: f64,
    pub min_rebalance_units: f64,
    pub current_exposure: &'a Exposure,
    pub desired_exposure: &'a Exposure,
    pub execution_quote: Option<ExecutionQuote>,
    pub submit_purpose: SubmitPurpose,
    pub exposure_epsilon: f64,
    pub curve_maker_levels_per_side: usize,
}

#[derive(Debug, Clone)]
pub(super) struct DesiredBinding {
    pub(super) proposal: BindingProposal,
    pub(super) allocations: Vec<BindingOperationAllocation>,
    pub(super) request: OrderRequest,
    pub(super) desired_exposure: Exposure,
    pub(super) submit_purpose: SubmitPurpose,
    pub(super) policy_state: BindingPolicyState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BindingReconciliationDecision {
    CoveredByExisting { indexes: Vec<usize> },
    ReuseExisting { index: usize },
    ReplaceReusable { index: usize },
    ReplaceActiveOwners { indexes: Vec<usize> },
    BlockedByActiveOwner,
    SubmitNew,
}

pub(super) fn plan_policy_bindings(
    context: PolicyContext,
    input: &PolicyPlanningInput<'_>,
) -> Vec<DesiredBinding> {
    match context {
        PolicyContext::Normal => plan_normal_policy_bindings(input),
        PolicyContext::ManualOverride => plan_manual_override_binding(input).into_iter().collect(),
        PolicyContext::ReduceOnly => plan_reduce_only_binding(input).into_iter().collect(),
    }
}

fn plan_normal_policy_bindings(input: &PolicyPlanningInput<'_>) -> Vec<DesiredBinding> {
    let mut covered_operations = BTreeSet::new();
    let mut desired_bindings = Vec::new();
    let gap_direction = direction_for_gap(&input.current_exposure.delta(input.desired_exposure));
    let catch_up_operations = select_catch_up_operations(
        input.view,
        &covered_operations,
        input.exposure_epsilon,
        gap_direction,
    );
    if let Some(binding) = plan_target_binding(
        input,
        PolicyKind::CatchUp,
        catch_up_operations.clone(),
        BindingPolicyState::Stateless,
    ) {
        desired_bindings.push(binding);
    }
    covered_operations.extend(catch_up_operations.iter().cloned());
    desired_bindings.extend(
        select_curve_maker_operations(
            input.view,
            &covered_operations,
            input.exposure_epsilon,
            input.curve_maker_levels_per_side,
        )
        .into_iter()
        .filter_map(|operation| plan_curve_maker_binding(input, operation)),
    );

    desired_bindings
}

fn plan_manual_override_binding(input: &PolicyPlanningInput<'_>) -> Option<DesiredBinding> {
    let direction = direction_for_gap(&input.current_exposure.delta(input.desired_exposure))?;
    let selected = select_target_operations(
        input.view,
        &BTreeSet::new(),
        direction,
        input.exposure_epsilon,
        false,
    );
    plan_target_binding(
        input,
        PolicyKind::ManualOverride,
        selected,
        BindingPolicyState::Stateless,
    )
}

fn plan_reduce_only_binding(input: &PolicyPlanningInput<'_>) -> Option<DesiredBinding> {
    if !target_change_decreases_inventory(input.current_exposure, input.desired_exposure) {
        return None;
    }
    let direction = direction_for_gap(&input.current_exposure.delta(input.desired_exposure))?;
    let selected = select_target_operations(
        input.view,
        &BTreeSet::new(),
        direction,
        input.exposure_epsilon,
        false,
    );
    plan_target_binding(
        input,
        PolicyKind::ReduceOnly,
        selected,
        BindingPolicyState::Stateless,
    )
}

pub(super) fn classify_binding_reconciliation(
    bindings: &[LiveOrderBinding],
    active_bindings: &[LiveOrderBinding],
    desired: &DesiredBinding,
    exchange_rules: &ExchangeRules,
    observed_at: chrono::DateTime<chrono::Utc>,
    curve_maker_grace_ms: i64,
) -> BindingReconciliationDecision {
    if has_cancel_pending_owner(active_bindings, &desired.allocations) {
        return BindingReconciliationDecision::CoveredByExisting {
            indexes: non_cancel_pending_owner_indexes_for_allocations(
                bindings,
                &desired.allocations,
            ),
        };
    }

    if desired.proposal.policy == PolicyKind::CatchUp {
        let indexes = existing_passive_covering_indexes(
            bindings,
            desired,
            exchange_rules,
            observed_at,
            curve_maker_grace_ms,
        );
        if !indexes.is_empty() {
            return BindingReconciliationDecision::CoveredByExisting { indexes };
        }
    }

    if let Some(index) =
        find_reusable_binding_by_proposal_key(bindings, &desired.proposal.proposal_key())
    {
        return if binding_request_matches_desired(&bindings[index], desired, exchange_rules) {
            BindingReconciliationDecision::ReuseExisting { index }
        } else {
            BindingReconciliationDecision::ReplaceReusable { index }
        };
    }

    if desired.proposal.policy == PolicyKind::CatchUp {
        let indexes = replaceable_owner_indexes(active_bindings, &desired.allocations);
        if !indexes.is_empty() {
            return BindingReconciliationDecision::ReplaceActiveOwners { indexes };
        }
    }

    if has_active_owner(active_bindings, &desired.allocations) {
        return BindingReconciliationDecision::BlockedByActiveOwner;
    }

    BindingReconciliationDecision::SubmitNew
}

pub(super) fn plan_target_binding(
    input: &PolicyPlanningInput<'_>,
    policy: PolicyKind,
    selected: Vec<BoundaryOperation>,
    policy_state: BindingPolicyState,
) -> Option<DesiredBinding> {
    let inventory_gap = input.current_exposure.delta(input.desired_exposure);
    if inventory_gap.0.abs() < input.min_rebalance_units {
        return None;
    }

    let direction = direction_for_gap(&inventory_gap)?;
    let price = execution_price(direction, input.execution_quote)?;
    let allocations = allocate_operations(input.view, selected, inventory_gap.0.abs());
    if allocations.is_empty() {
        return None;
    }

    let exposure_qty = allocations
        .iter()
        .map(|allocation| allocation.exposure_qty)
        .sum::<f64>();
    let quantity = round_to_step(
        exposure_qty * input.base_qty_per_unit,
        input.exchange_rules.quantity_step,
    );
    if quantity <= f64::EPSILON || !is_meetable_minimum(price, quantity, input.exchange_rules) {
        return None;
    }

    let request = OrderRequest {
        instrument: input.instrument.clone(),
        side: side_for_direction(direction),
        price,
        quantity,
        client_order_id: next_client_order_id(policy),
        reduce_only: target_change_decreases_inventory(
            input.current_exposure,
            input.desired_exposure,
        ),
    };
    let proposal = proposal_for_allocations(policy, &allocations);
    Some(DesiredBinding {
        proposal,
        allocations,
        request,
        desired_exposure: input.desired_exposure.clone(),
        submit_purpose: input.submit_purpose,
        policy_state,
    })
}

fn select_catch_up_operations(
    view: &BoundaryLedgerView,
    covered_operations: &BTreeSet<BoundaryOperation>,
    exposure_epsilon: f64,
    gap_direction: Option<BoundaryDirection>,
) -> Vec<BoundaryOperation> {
    let mut up = select_target_operations(
        view,
        covered_operations,
        BoundaryDirection::Up,
        exposure_epsilon,
        true,
    );
    let down = select_target_operations(
        view,
        covered_operations,
        BoundaryDirection::Down,
        exposure_epsilon,
        true,
    );
    up.extend(down);
    up.retain(|operation| Some(operation.direction) == gap_direction);
    up
}

fn select_curve_maker_operations(
    view: &BoundaryLedgerView,
    covered_operations: &BTreeSet<BoundaryOperation>,
    exposure_epsilon: f64,
    levels_per_side: usize,
) -> Vec<BoundaryOperation> {
    let mut up = view
        .operations
        .iter()
        .filter(|operation| !operation.due)
        .filter(|operation| operation.remaining > exposure_epsilon)
        .filter(|operation| {
            operation.operation.direction == crate::executor::boundary::BoundaryDirection::Up
        })
        .filter(|operation| !covered_operations.contains(&operation.operation))
        .map(|operation| operation.operation.clone())
        .take(levels_per_side)
        .collect::<Vec<_>>();
    let mut down = view
        .operations
        .iter()
        .rev()
        .filter(|operation| !operation.due)
        .filter(|operation| operation.remaining > exposure_epsilon)
        .filter(|operation| {
            operation.operation.direction == crate::executor::boundary::BoundaryDirection::Down
        })
        .filter(|operation| !covered_operations.contains(&operation.operation))
        .map(|operation| operation.operation.clone())
        .take(levels_per_side)
        .collect::<Vec<_>>();

    up.append(&mut down);
    up
}

pub(super) fn select_target_operations(
    view: &BoundaryLedgerView,
    covered_operations: &BTreeSet<BoundaryOperation>,
    direction: BoundaryDirection,
    exposure_epsilon: f64,
    require_due: bool,
) -> Vec<BoundaryOperation> {
    let mut selected = view
        .operations
        .iter()
        .filter(|operation| operation.operation.direction == direction)
        .filter(|operation| !require_due || operation.due)
        .filter(|operation| operation.remaining > exposure_epsilon)
        .filter(|operation| !covered_operations.contains(&operation.operation))
        .map(|operation| operation.operation.clone())
        .collect::<Vec<_>>();
    if direction == BoundaryDirection::Down {
        selected.reverse();
    }
    selected
}

fn find_reusable_binding_by_proposal_key(
    bindings: &[LiveOrderBinding],
    proposal_key: &crate::executor::binding::BindingProposalKey,
) -> Option<usize> {
    bindings
        .iter()
        .enumerate()
        .rev()
        .find(|(_index, binding)| {
            binding_is_active(binding)
                && binding.status != BindingStatus::CancelPending
                && &binding.proposal_key == proposal_key
        })
        .map(|(index, _binding)| index)
}

fn has_cancel_pending_owner(
    bindings: &[LiveOrderBinding],
    allocations: &[BindingOperationAllocation],
) -> bool {
    bindings
        .iter()
        .filter(|binding| binding.status == BindingStatus::CancelPending)
        .any(|binding| allocations_overlap(&binding.allocations, allocations))
}

fn non_cancel_pending_owner_indexes_for_allocations(
    bindings: &[LiveOrderBinding],
    allocations: &[BindingOperationAllocation],
) -> Vec<usize> {
    bindings
        .iter()
        .enumerate()
        .filter(|(_index, binding)| {
            binding_is_active(binding)
                && binding.status != BindingStatus::CancelPending
                && allocations_overlap(&binding.allocations, allocations)
        })
        .map(|(index, _binding)| index)
        .collect()
}

fn existing_passive_covering_indexes(
    bindings: &[LiveOrderBinding],
    desired: &DesiredBinding,
    exchange_rules: &ExchangeRules,
    observed_at: chrono::DateTime<chrono::Utc>,
    curve_maker_grace_ms: i64,
) -> Vec<usize> {
    let mut matched_indexes = BTreeSet::new();
    for desired_allocation in &desired.allocations {
        let Some((index, _binding)) = bindings.iter().enumerate().find(|(_index, binding)| {
            binding_is_passive_covering_owner(
                binding,
                desired,
                desired_allocation,
                exchange_rules,
                observed_at,
                curve_maker_grace_ms,
            )
        }) else {
            return Vec::new();
        };
        matched_indexes.insert(index);
    }
    matched_indexes.into_iter().collect()
}

fn binding_is_passive_covering_owner(
    binding: &LiveOrderBinding,
    desired: &DesiredBinding,
    desired_allocation: &BindingOperationAllocation,
    exchange_rules: &ExchangeRules,
    observed_at: chrono::DateTime<chrono::Utc>,
    curve_maker_grace_ms: i64,
) -> bool {
    if binding.proposal_key.policy != PolicyKind::CurveMaker
        || !binding_is_active(binding)
        || binding.status == BindingStatus::CancelPending
        || curve_maker_grace_expired(binding, observed_at, curve_maker_grace_ms)
        || binding.request.side != desired.request.side
        || binding.request.reduce_only != desired.request.reduce_only
        || !values_match(
            binding.request.price,
            desired.request.price,
            exchange_rules.price_tick,
        )
    {
        return false;
    }

    binding.allocations.iter().any(|allocation| {
        allocation.operation == desired_allocation.operation
            && (allocation.exposure_qty - binding.absorbed_exposure_qty).max(0.0)
                + exposure_quantity_epsilon()
                >= desired_allocation.exposure_qty
    })
}

fn curve_maker_grace_expired(
    binding: &LiveOrderBinding,
    observed_at: chrono::DateTime<chrono::Utc>,
    grace_ms: i64,
) -> bool {
    let BindingPolicyState::CurveMaker {
        due_grace_started_at: Some(started_at),
    } = binding.policy_state
    else {
        return false;
    };
    observed_at
        .signed_duration_since(started_at)
        .num_milliseconds()
        >= grace_ms
}

fn has_active_owner(
    bindings: &[LiveOrderBinding],
    allocations: &[BindingOperationAllocation],
) -> bool {
    allocations.iter().any(|desired_allocation| {
        bindings
            .iter()
            .filter(|binding| {
                binding_is_active(binding) && binding.status != BindingStatus::CancelPending
            })
            .any(|binding| {
                binding
                    .allocations
                    .iter()
                    .any(|allocation| allocation.operation == desired_allocation.operation)
            })
    })
}

fn replaceable_owner_indexes(
    bindings: &[LiveOrderBinding],
    allocations: &[BindingOperationAllocation],
) -> Vec<usize> {
    bindings
        .iter()
        .enumerate()
        .filter_map(|(index, binding)| {
            if binding.proposal_key.policy == PolicyKind::CatchUp
                || !binding_is_active(binding)
                || binding.status == BindingStatus::CancelPending
                || !allocations_overlap(&binding.allocations, allocations)
            {
                return None;
            }
            Some(index)
        })
        .collect()
}

fn allocations_overlap(
    left: &[BindingOperationAllocation],
    right: &[BindingOperationAllocation],
) -> bool {
    left.iter().any(|left_allocation| {
        right
            .iter()
            .any(|right_allocation| left_allocation.operation == right_allocation.operation)
    })
}

fn binding_request_matches_desired(
    binding: &LiveOrderBinding,
    desired: &DesiredBinding,
    exchange_rules: &ExchangeRules,
) -> bool {
    binding.request.instrument == desired.request.instrument
        && binding.request.side == desired.request.side
        && values_match(
            binding.request.price,
            desired.request.price,
            exchange_rules.price_tick,
        )
        && values_match(
            binding.request.quantity,
            desired.request.quantity,
            exchange_rules.quantity_step,
        )
        && binding.request.reduce_only == desired.request.reduce_only
}

fn values_match(left: f64, right: f64, tolerance: f64) -> bool {
    let tolerance = tolerance.max(f64::EPSILON);
    (left - right).abs() <= tolerance + f64::EPSILON
}

fn exposure_quantity_epsilon() -> f64 {
    1e-9
}

fn plan_curve_maker_binding(
    input: &PolicyPlanningInput<'_>,
    operation: BoundaryOperation,
) -> Option<DesiredBinding> {
    let boundary = boundary_for_operation(input.boundaries, &operation)?;
    let price = maker_price_for_operation(boundary, &operation, input)?;
    let operation_view = input
        .view
        .operations
        .iter()
        .find(|candidate| candidate.operation == operation)?;
    let quantity = round_to_step(
        operation_view.remaining * input.base_qty_per_unit,
        input.exchange_rules.quantity_step,
    );
    if quantity <= f64::EPSILON || !is_meetable_minimum(price, quantity, input.exchange_rules) {
        return None;
    }

    let request = OrderRequest {
        instrument: input.instrument.clone(),
        side: side_for_direction(operation.direction),
        price,
        quantity,
        client_order_id: next_client_order_id(PolicyKind::CurveMaker),
        reduce_only: reduce_only_for_operation(boundary, operation.direction),
    };
    let allocations = vec![BindingOperationAllocation {
        operation,
        exposure_qty: operation_view.remaining,
    }];
    let proposal = proposal_for_allocations(PolicyKind::CurveMaker, &allocations);
    Some(DesiredBinding {
        proposal,
        allocations,
        request,
        desired_exposure: input.desired_exposure.clone(),
        submit_purpose: input.submit_purpose,
        policy_state: BindingPolicyState::CurveMaker {
            due_grace_started_at: None,
        },
    })
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

fn boundary_for_operation<'a>(
    boundaries: &'a [BoundaryBlueprint],
    operation: &BoundaryOperation,
) -> Option<&'a BoundaryBlueprint> {
    boundaries
        .iter()
        .find(|boundary| boundary.id == operation.boundary_id)
}

fn maker_price_for_operation(
    boundary: &BoundaryBlueprint,
    operation: &BoundaryOperation,
    input: &PolicyPlanningInput<'_>,
) -> Option<f64> {
    let raw_price = match operation.direction {
        BoundaryDirection::Up => boundary.trigger_price,
        BoundaryDirection::Down => {
            trigger_price_for_boundary(boundary.lower_exposure.0, input.config)
        }
    };
    raw_price.is_finite().then(|| {
        round_passive_price(
            raw_price,
            input.exchange_rules.price_tick,
            operation.direction,
        )
    })
}

fn round_passive_price(price: f64, tick: f64, direction: BoundaryDirection) -> f64 {
    if tick <= f64::EPSILON {
        return price;
    }
    match direction {
        BoundaryDirection::Up => (price / tick).floor() * tick,
        BoundaryDirection::Down => (price / tick).ceil() * tick,
    }
}

pub(super) fn direction_for_gap(gap: &Exposure) -> Option<BoundaryDirection> {
    if gap.0 > f64::EPSILON {
        Some(BoundaryDirection::Up)
    } else if gap.0 < -f64::EPSILON {
        Some(BoundaryDirection::Down)
    } else {
        None
    }
}

fn reduce_only_for_operation(boundary: &BoundaryBlueprint, direction: BoundaryDirection) -> bool {
    let (from, to) = match direction {
        BoundaryDirection::Up => (boundary.lower_exposure.0, boundary.upper_exposure.0),
        BoundaryDirection::Down => (boundary.upper_exposure.0, boundary.lower_exposure.0),
    };
    to.abs() + f64::EPSILON < from.abs()
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

fn target_change_decreases_inventory(
    current_exposure: &Exposure,
    desired_exposure: &Exposure,
) -> bool {
    desired_exposure.0.abs() + f64::EPSILON < current_exposure.0.abs()
}

fn proposal_for_allocations(
    policy: PolicyKind,
    allocations: &[BindingOperationAllocation],
) -> BindingProposal {
    BindingProposal {
        policy,
        operations: allocations
            .iter()
            .map(|allocation| allocation.operation.clone())
            .collect(),
    }
}

fn next_client_order_id(policy: PolicyKind) -> String {
    let instance_id = Uuid::new_v4().simple();
    match policy {
        PolicyKind::ManualOverride => format!("bo-{instance_id}"),
        PolicyKind::ReduceOnly => format!("br-{instance_id}"),
        PolicyKind::CatchUp => format!("bc-{instance_id}"),
        PolicyKind::CurveMaker => format!("bk-{instance_id}"),
    }
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure};

    use crate::executor::boundary::{
        BoundaryDirection, BoundaryId, BoundaryOperation, ProfileRevision, discretize_boundaries,
    };
    use crate::executor::ledger::{BoundaryLedgerView, BoundaryOperationView};
    use crate::ports::ExecutionQuote;
    use crate::price_gate::SubmitPurpose;
    use poise_core::track::{Instrument, Venue};

    use super::*;

    fn operation(lower_bp: i64, upper_bp: i64, direction: BoundaryDirection) -> BoundaryOperation {
        BoundaryOperation {
            boundary_id: BoundaryId {
                profile_revision: ProfileRevision("rev-1".to_string()),
                lower_exposure_bp: lower_bp,
                upper_exposure_bp: upper_bp,
            },
            direction,
        }
    }

    fn config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 2.0,
            short_exposure_units: 2.0,
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

    #[test]
    fn catch_up_policy_selects_due_uncovered_operations_only() {
        let due = operation(0, 10_000, BoundaryDirection::Up);
        let future = operation(10_000, 20_000, BoundaryDirection::Up);
        let covered = operation(20_000, 30_000, BoundaryDirection::Up);
        let view = BoundaryLedgerView {
            operations: vec![
                BoundaryOperationView {
                    operation: due.clone(),
                    remaining: 1.0,
                    due: true,
                },
                BoundaryOperationView {
                    operation: future,
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: covered.clone(),
                    remaining: 1.0,
                    due: true,
                },
            ],
        };
        let coverage = BTreeSet::from([covered]);

        let selected =
            select_catch_up_operations(&view, &coverage, 1e-9, Some(BoundaryDirection::Up));

        assert_eq!(selected, vec![due]);
    }

    #[test]
    fn target_selection_prefers_nearest_down_operations_first() {
        let far = operation(10_000, 20_000, BoundaryDirection::Down);
        let near = operation(0, 10_000, BoundaryDirection::Down);
        let view = BoundaryLedgerView {
            operations: vec![
                BoundaryOperationView {
                    operation: near.clone(),
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: far.clone(),
                    remaining: 1.0,
                    due: false,
                },
            ],
        };

        let selected = select_target_operations(
            &view,
            &BTreeSet::new(),
            BoundaryDirection::Down,
            1e-9,
            false,
        );

        assert_eq!(selected, vec![far, near]);
    }

    #[test]
    fn curve_maker_policy_selects_nearest_future_operations_per_side() {
        let future_up_near = operation(0, 10_000, BoundaryDirection::Up);
        let future_up_far = operation(10_000, 20_000, BoundaryDirection::Up);
        let future_down_far = operation(-20_000, -10_000, BoundaryDirection::Down);
        let future_down_near = operation(-10_000, 0, BoundaryDirection::Down);
        let due = operation(20_000, 30_000, BoundaryDirection::Up);
        let view = BoundaryLedgerView {
            operations: vec![
                BoundaryOperationView {
                    operation: future_down_far,
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: future_down_near.clone(),
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: future_up_near.clone(),
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: future_up_far,
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: due,
                    remaining: 1.0,
                    due: true,
                },
            ],
        };

        let selected = select_curve_maker_operations(&view, &BTreeSet::new(), 1e-9, 1);

        assert_eq!(selected, vec![future_up_near, future_down_near]);
    }

    #[test]
    fn boundary_policy_plans_catch_up_before_curve_maker_bindings() {
        let config = config();
        let rules = rules();
        let boundaries = discretize_boundaries(&config, ProfileRevision("rev-1".to_string()));
        let due_up = BoundaryOperation {
            boundary_id: boundaries[2].id.clone(),
            direction: BoundaryDirection::Up,
        };
        let future_up = BoundaryOperation {
            boundary_id: boundaries[3].id.clone(),
            direction: BoundaryDirection::Up,
        };
        let future_down = BoundaryOperation {
            boundary_id: boundaries[1].id.clone(),
            direction: BoundaryDirection::Down,
        };
        let view = BoundaryLedgerView {
            operations: vec![
                BoundaryOperationView {
                    operation: due_up.clone(),
                    remaining: 1.0,
                    due: true,
                },
                BoundaryOperationView {
                    operation: future_up.clone(),
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: future_down.clone(),
                    remaining: 1.0,
                    due: false,
                },
            ],
        };
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");

        let desired = plan_policy_bindings(
            PolicyContext::Normal,
            &PolicyPlanningInput {
                view: &view,
                boundaries: &boundaries,
                instrument: &instrument,
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: &Exposure(0.0),
                desired_exposure: &Exposure(1.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                submit_purpose: SubmitPurpose::AutoReconcile,
                exposure_epsilon: 1e-9,
                curve_maker_levels_per_side: 1,
            },
        );

        assert_eq!(
            desired
                .iter()
                .map(|binding| binding.proposal.policy)
                .collect::<Vec<_>>(),
            vec![
                PolicyKind::CatchUp,
                PolicyKind::CurveMaker,
                PolicyKind::CurveMaker
            ]
        );
        assert_eq!(desired[0].allocations[0].operation, due_up);
        assert_eq!(desired[1].allocations[0].operation, future_up);
        assert_eq!(desired[2].allocations[0].operation, future_down);
        assert_eq!(desired[0].allocations.len(), 1);
        assert_eq!(desired[1].allocations.len(), 1);
        assert_eq!(desired[2].allocations.len(), 1);
    }

    #[test]
    fn policy_uses_exchange_safe_client_order_id_prefixes() {
        for (policy, expected_prefix) in [
            (PolicyKind::ManualOverride, "bo-"),
            (PolicyKind::ReduceOnly, "br-"),
            (PolicyKind::CatchUp, "bc-"),
            (PolicyKind::CurveMaker, "bk-"),
        ] {
            let client_order_id = next_client_order_id(policy);
            assert!(
                client_order_id.len() < 36,
                "Binance requires client order ids shorter than 36 chars, got `{}` with len {}",
                client_order_id,
                client_order_id.len()
            );
            assert!(
                client_order_id.starts_with(expected_prefix),
                "client order id should keep a compact policy prefix"
            );
        }
    }
}
