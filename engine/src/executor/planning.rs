use chrono::{DateTime, Utc};
use poise_core::strategy::TrackConfig;
use poise_core::types::{ExchangeRules, Exposure, Side};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

use crate::execution_plan::ExecutionAction;
use crate::execution_plan::{is_meetable_minimum, round_to_step};
use crate::ports::{ExecutionQuote, OrderRequest};
use crate::price_gate::{
    PriceExecutionGate, SubmitPurpose, WorkingOrderGateAction, allows_submit,
    working_order_gate_action,
};
use crate::runtime::ExecutorState;
use crate::track::Instrument;

use super::binding::{
    BindingOperationAllocation, BindingPolicyState, BindingProposal, BindingStatus,
    LiveOrderBinding, SubmitRecoveryToken, active_binding_exposure_budget,
};
use super::boundary::{
    BoundaryBlueprint, BoundaryDirection, BoundaryOperation, discretize_boundaries,
    profile_revision_for_config, trigger_price_for_boundary,
};
use super::ledger::BoundaryLedgerView;
use super::policy::{
    CoverageReservation, PolicyKind, select_catch_up_operations, select_curve_maker_operations,
    select_target_operations,
};
use super::recovery::RecoveryAnomaly;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderRole {
    IncreaseInventory,
    DecreaseInventory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyContext {
    Normal,
    ManualOverride,
    Flatten,
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
    pub policy_context: PolicyContext,
    pub price_execution_gate: PriceExecutionGate,
    pub submit_purpose: SubmitPurpose,
    pub observed_at: DateTime<Utc>,
}

pub struct ExecutorPlan {
    pub state: ExecutorState,
    #[allow(dead_code)]
    pub desired_bindings: Vec<BindingProposal>,
    pub effects: Vec<ExecutionAction>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSubmitHint {
    pub request: OrderRequest,
    pub desired_exposure: Exposure,
    pub submit_purpose: SubmitPurpose,
    pub recovery_token: SubmitRecoveryToken,
}

#[derive(Debug, Clone)]
struct DesiredBinding {
    proposal: BindingProposal,
    allocations: Vec<BindingOperationAllocation>,
    request: OrderRequest,
    desired_exposure: Exposure,
    submit_purpose: SubmitPurpose,
    policy_state: BindingPolicyState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BindingReconciliationDecision {
    CoveredByExisting { indexes: Vec<usize> },
    ReuseExisting { index: usize },
    ReplaceReusable { index: usize },
    ReplaceActiveOwners { indexes: Vec<usize> },
    BlockedByActiveOwner,
    SubmitNew,
}

const CURVE_MAKER_LEVELS_PER_SIDE: usize = 3;
const CURVE_MAKER_GRACE_MS: i64 = 60_000;
static BINDING_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

    if state.recovery_anomaly.is_some() {
        return noop_plan(state);
    }

    let boundaries = discretize_boundaries(
        submit_intent.config,
        profile_revision_for_config(submit_intent.config),
    );
    let exposure_epsilon = exposure_epsilon(&submit_intent);
    let view = match BoundaryLedgerView::try_from_boundaries(
        &boundaries,
        &state.ledger_state,
        submit_intent.desired_exposure.clone(),
        exposure_epsilon,
    ) {
        Ok(view) => view,
        Err(_error) => {
            state.recovery_anomaly = Some(RecoveryAnomaly::BoundaryProgressOutOfRange);
            return noop_plan(state);
        }
    };
    if state.ledger_state.has_unexplained_exposure_drift(
        &submit_intent.current_exposure,
        active_binding_exposure_budget(&state.bindings),
        exposure_epsilon,
    ) {
        state.recovery_anomaly = Some(RecoveryAnomaly::ExpectedExposureMismatch);
        return noop_plan(state);
    }

    // IMPORTANT: Capture cancel-pending set BEFORE update_curve_maker_due_grace.
    // Grace-triggered cancels must NOT appear in this set, otherwise CatchUp bindings
    // would be blocked from submitting in the same reconcile round as the maker cancel.
    // See test: catch_up_policy_takes_over_due_curve_maker_in_same_round
    let preexisting_cancel_pending_operations = cancel_pending_operations(&state.bindings);
    let mut effects =
        apply_price_gate_to_existing_bindings(&mut state, submit_intent.price_execution_gate);

    if !allows_submit(
        submit_intent.price_execution_gate,
        submit_intent.submit_purpose,
    ) {
        return if effects.is_empty() {
            noop_plan(state)
        } else {
            ExecutorPlan {
                state,
                desired_bindings: Vec::new(),
                effects,
            }
        };
    }

    if submit_intent.policy_context == PolicyContext::Normal {
        effects.extend(update_curve_maker_due_grace(
            &mut state,
            &view,
            submit_intent.observed_at,
            CURVE_MAKER_GRACE_MS,
        ));
    }
    let mut desired_bindings = Vec::new();
    let mut desired_coverage = CoverageReservation::default();

    // Policy priority is enforced in two layers:
    //   1. Outer layer (manager.rs): PolicyContext decides ManualOverride/Flatten vs Normal.
    //      When the track is in Manual or Flatten state, only that policy runs.
    //   2. Inner layer (here): Within Normal, CatchUp runs before CurveMaker.
    //      CatchUp reserves its operations via CoverageReservation, so CurveMaker
    //      skips already-claimed operations.
    // Effective priority: ManualOverride > Flatten > CatchUp > CurveMaker.
    match submit_intent.policy_context {
        PolicyContext::Normal => {
            if let Some(desired) = plan_catch_up_binding(&submit_intent, &view, &desired_coverage) {
                for allocation in &desired.allocations {
                    desired_coverage.reserve(allocation.operation.clone());
                }
                desired_bindings.push(desired);
            }

            for desired in
                plan_curve_maker_bindings(&submit_intent, &boundaries, &view, &desired_coverage)
            {
                for allocation in &desired.allocations {
                    desired_coverage.reserve(allocation.operation.clone());
                }
                desired_bindings.push(desired);
            }
        }
        PolicyContext::ManualOverride => {
            if let Some(desired) =
                plan_manual_override_binding(&submit_intent, &view, &desired_coverage)
            {
                desired_bindings.push(desired);
            }
        }
        PolicyContext::Flatten => {
            if let Some(desired) = plan_flatten_binding(&submit_intent, &view, &desired_coverage) {
                desired_bindings.push(desired);
            }
        }
    }
    effects.extend(reconcile_bindings(
        &mut state,
        &desired_bindings,
        submit_intent.exchange_rules,
        &preexisting_cancel_pending_operations,
    ));

    if effects.is_empty() {
        effects.push(ExecutionAction::NoOp);
    }

    // Clean up terminal bindings to prevent unbounded growth of the bindings Vec.
    // This must happen AFTER reconcile_bindings, which may reference just-terminated bindings.
    state
        .bindings
        .retain(|binding| binding.status != BindingStatus::Terminal);

    ExecutorPlan {
        state,
        desired_bindings: desired_bindings
            .iter()
            .map(|binding| binding.proposal.clone())
            .collect(),
        effects,
    }
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
    }
}

fn plan_catch_up_binding(
    submit_intent: &SubmitIntentInput<'_>,
    view: &BoundaryLedgerView,
    coverage: &CoverageReservation,
) -> Option<DesiredBinding> {
    let selected = select_catch_up_operations(view, coverage, exposure_epsilon(submit_intent))
        .into_iter()
        .filter(|operation| {
            Some(operation.direction)
                == direction_for_gap(
                    &submit_intent
                        .current_exposure
                        .delta(&submit_intent.desired_exposure),
                )
        })
        .collect::<Vec<_>>();
    plan_target_binding(
        submit_intent,
        view,
        PolicyKind::CatchUp,
        selected,
        BindingPolicyState::Stateless,
    )
}

fn plan_manual_override_binding(
    submit_intent: &SubmitIntentInput<'_>,
    view: &BoundaryLedgerView,
    coverage: &CoverageReservation,
) -> Option<DesiredBinding> {
    let direction = direction_for_gap(
        &submit_intent
            .current_exposure
            .delta(&submit_intent.desired_exposure),
    )?;
    let selected = select_target_operations(
        view,
        coverage,
        direction,
        exposure_epsilon(submit_intent),
        false,
    );
    plan_target_binding(
        submit_intent,
        view,
        PolicyKind::ManualOverride,
        selected,
        BindingPolicyState::Stateless,
    )
}

fn plan_flatten_binding(
    submit_intent: &SubmitIntentInput<'_>,
    view: &BoundaryLedgerView,
    coverage: &CoverageReservation,
) -> Option<DesiredBinding> {
    if role_for_target_change(
        &submit_intent.current_exposure,
        &submit_intent.desired_exposure,
    ) != OrderRole::DecreaseInventory
    {
        return None;
    }
    let direction = direction_for_gap(
        &submit_intent
            .current_exposure
            .delta(&submit_intent.desired_exposure),
    )?;
    let selected = select_target_operations(
        view,
        coverage,
        direction,
        exposure_epsilon(submit_intent),
        false,
    );
    plan_target_binding(
        submit_intent,
        view,
        PolicyKind::Flatten,
        selected,
        BindingPolicyState::Stateless,
    )
}

fn plan_target_binding(
    submit_intent: &SubmitIntentInput<'_>,
    view: &BoundaryLedgerView,
    policy: PolicyKind,
    selected: Vec<BoundaryOperation>,
    policy_state: BindingPolicyState,
) -> Option<DesiredBinding> {
    let inventory_gap = submit_intent
        .current_exposure
        .delta(&submit_intent.desired_exposure);
    if inventory_gap.0.abs() < submit_intent.min_rebalance_units {
        return None;
    }

    let direction = direction_for_gap(&inventory_gap)?;
    let price = execution_price(direction, submit_intent.execution_quote)?;
    let allocations = allocate_operations(view, selected, inventory_gap.0.abs());
    if allocations.is_empty() {
        return None;
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
        return None;
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
        client_order_id: next_client_order_id(policy),
        reduce_only: role == OrderRole::DecreaseInventory,
    };
    let proposal = proposal_for_allocations(policy, &allocations);
    Some(DesiredBinding {
        proposal,
        allocations,
        request,
        desired_exposure: submit_intent.desired_exposure.clone(),
        submit_purpose: submit_intent.submit_purpose,
        policy_state,
    })
}

fn plan_curve_maker_bindings(
    submit_intent: &SubmitIntentInput<'_>,
    boundaries: &[BoundaryBlueprint],
    view: &BoundaryLedgerView,
    coverage: &CoverageReservation,
) -> Vec<DesiredBinding> {
    select_curve_maker_operations(
        view,
        coverage,
        exposure_epsilon(submit_intent),
        CURVE_MAKER_LEVELS_PER_SIDE,
    )
    .into_iter()
    .filter_map(|operation| {
        let boundary = boundary_for_operation(boundaries, &operation)?;
        let price = maker_price_for_operation(boundary, &operation, submit_intent)?;
        let operation_view = view
            .operations
            .iter()
            .find(|candidate| candidate.operation == operation)?;
        let quantity = round_to_step(
            operation_view.remaining * submit_intent.base_qty_per_unit,
            submit_intent.exchange_rules.quantity_step,
        );
        if quantity <= f64::EPSILON
            || !is_meetable_minimum(price, quantity, submit_intent.exchange_rules)
        {
            return None;
        }

        let request = OrderRequest {
            instrument: submit_intent.instrument.clone(),
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
            desired_exposure: submit_intent.desired_exposure.clone(),
            submit_purpose: submit_intent.submit_purpose,
            policy_state: BindingPolicyState::CurveMaker {
                due_grace_started_at: None,
            },
        })
    })
    .collect()
}

fn update_curve_maker_due_grace(
    state: &mut ExecutorState,
    view: &BoundaryLedgerView,
    observed_at: DateTime<Utc>,
    grace_ms: i64,
) -> Vec<ExecutionAction> {
    let mut effects = Vec::new();
    for binding in &mut state.bindings {
        if binding.proposal_key.policy != PolicyKind::CurveMaker
            || !matches!(
                binding.status,
                BindingStatus::SubmitPending | BindingStatus::Working
            )
        {
            continue;
        }
        let BindingPolicyState::CurveMaker {
            due_grace_started_at,
        } = &mut binding.policy_state
        else {
            continue;
        };
        let is_due = binding
            .allocations
            .iter()
            .any(|allocation| view.is_due(&allocation.operation));
        if !is_due {
            *due_grace_started_at = None;
            continue;
        }

        let started_at = *due_grace_started_at.get_or_insert(observed_at);
        if observed_at
            .signed_duration_since(started_at)
            .num_milliseconds()
            < grace_ms
        {
            continue;
        }

        binding.status = BindingStatus::CancelPending;
        if let Some(order_id) = binding.order_id.clone() {
            effects.push(ExecutionAction::CancelOrder {
                instrument: binding.request.instrument.clone(),
                order_id,
            });
        } else {
            binding.status = BindingStatus::Terminal;
        }
    }
    effects
}

fn apply_price_gate_to_existing_bindings(
    state: &mut ExecutorState,
    gate: PriceExecutionGate,
) -> Vec<ExecutionAction> {
    let mut effects = Vec::new();
    for binding in &mut state.bindings {
        if !matches!(
            binding.status,
            BindingStatus::SubmitPending | BindingStatus::Working
        ) {
            continue;
        }
        match working_order_gate_action(gate, order_role_for_binding(binding)) {
            WorkingOrderGateAction::Keep => {}
            WorkingOrderGateAction::Cancel => cancel_binding(binding, &mut effects),
        }
    }
    effects
}

fn order_role_for_binding(binding: &LiveOrderBinding) -> OrderRole {
    if binding.request.reduce_only {
        OrderRole::DecreaseInventory
    } else {
        OrderRole::IncreaseInventory
    }
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

fn reconcile_bindings(
    state: &mut ExecutorState,
    desired_bindings: &[DesiredBinding],
    exchange_rules: &ExchangeRules,
    preexisting_cancel_pending_operations: &BTreeSet<BoundaryOperation>,
) -> Vec<ExecutionAction> {
    let mut effects = Vec::new();
    let existing_count = state.bindings.len();
    let mut matched_existing = BTreeSet::new();

    for desired in desired_bindings {
        match classify_binding_reconciliation(
            &state.bindings[..existing_count],
            &state.bindings,
            desired,
            exchange_rules,
            preexisting_cancel_pending_operations,
        ) {
            BindingReconciliationDecision::CoveredByExisting { indexes } => {
                matched_existing.extend(indexes);
            }
            BindingReconciliationDecision::ReuseExisting { index } => {
                matched_existing.insert(index);
                update_existing_binding_from_desired(&mut state.bindings[index], desired);
            }
            BindingReconciliationDecision::ReplaceReusable { index } => {
                matched_existing.insert(index);
                cancel_binding(&mut state.bindings[index], &mut effects);
                submit_desired_binding(state, desired, &mut effects);
            }
            BindingReconciliationDecision::ReplaceActiveOwners { indexes } => {
                for index in indexes {
                    cancel_binding(&mut state.bindings[index], &mut effects);
                }
                submit_desired_binding(state, desired, &mut effects);
            }
            BindingReconciliationDecision::BlockedByActiveOwner => {}
            BindingReconciliationDecision::SubmitNew => {
                submit_desired_binding(state, desired, &mut effects);
            }
        }
    }

    for index in 0..existing_count {
        if matched_existing.contains(&index) {
            continue;
        }
        let binding = &mut state.bindings[index];
        if !binding_is_active(binding) {
            continue;
        }
        cancel_binding(binding, &mut effects);
    }

    effects
}

fn classify_binding_reconciliation(
    bindings: &[LiveOrderBinding],
    active_bindings: &[LiveOrderBinding],
    desired: &DesiredBinding,
    exchange_rules: &ExchangeRules,
    cancel_pending_operations: &BTreeSet<BoundaryOperation>,
) -> BindingReconciliationDecision {
    if has_cancel_pending_owner(cancel_pending_operations, &desired.allocations) {
        return BindingReconciliationDecision::CoveredByExisting {
            indexes: active_owner_indexes_for_allocations(bindings, &desired.allocations),
        };
    }

    if desired.proposal.policy == PolicyKind::CatchUp {
        let indexes = effective_maker_owner_indexes(bindings, desired, exchange_rules);
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
        let indexes = replaceable_active_owner_indexes(active_bindings, &desired.allocations);
        if !indexes.is_empty() {
            return BindingReconciliationDecision::ReplaceActiveOwners { indexes };
        }
    }

    if has_active_owner(active_bindings, &desired.allocations) {
        return BindingReconciliationDecision::BlockedByActiveOwner;
    }

    BindingReconciliationDecision::SubmitNew
}

fn cancel_pending_operations(bindings: &[LiveOrderBinding]) -> BTreeSet<BoundaryOperation> {
    bindings
        .iter()
        .filter(|binding| binding.status == BindingStatus::CancelPending)
        .flat_map(|binding| {
            binding
                .allocations
                .iter()
                .map(|allocation| allocation.operation.clone())
        })
        .collect()
}

fn active_owner_indexes_for_allocations(
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

fn has_cancel_pending_owner(
    cancel_pending_operations: &BTreeSet<BoundaryOperation>,
    allocations: &[BindingOperationAllocation],
) -> bool {
    allocations
        .iter()
        .any(|allocation| cancel_pending_operations.contains(&allocation.operation))
}

fn effective_maker_owner_indexes(
    bindings: &[LiveOrderBinding],
    desired: &DesiredBinding,
    exchange_rules: &ExchangeRules,
) -> Vec<usize> {
    let mut matched_indexes = BTreeSet::new();
    for desired_allocation in &desired.allocations {
        let Some((index, _binding)) = bindings.iter().enumerate().find(|(_index, binding)| {
            binding_is_effective_maker_owner(binding, desired, desired_allocation, exchange_rules)
        }) else {
            return Vec::new();
        };
        matched_indexes.insert(index);
    }
    matched_indexes.into_iter().collect()
}

fn binding_is_effective_maker_owner(
    binding: &LiveOrderBinding,
    desired: &DesiredBinding,
    desired_allocation: &BindingOperationAllocation,
    exchange_rules: &ExchangeRules,
) -> bool {
    if binding.proposal_key.policy != PolicyKind::CurveMaker
        || !binding_is_active(binding)
        || binding.status == BindingStatus::CancelPending
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

fn desired_binding_from_spec(desired: &DesiredBinding) -> LiveOrderBinding {
    let proposal_key = desired.proposal.proposal_key();
    LiveOrderBinding {
        binding_id: next_binding_id(proposal_key.policy),
        proposal_key,
        allocations: desired.allocations.clone(),
        absorbed_exposure_qty: 0.0,
        request: desired.request.clone(),
        desired_exposure: desired.desired_exposure.clone(),
        submit_purpose: desired.submit_purpose,
        order_id: None,
        status: BindingStatus::SubmitPending,
        policy_state: desired.policy_state.clone(),
    }
}

fn find_reusable_binding_by_proposal_key(
    bindings: &[LiveOrderBinding],
    proposal_key: &super::binding::BindingProposalKey,
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

fn binding_is_active(binding: &LiveOrderBinding) -> bool {
    super::binding::binding_is_active(binding)
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

fn replaceable_active_owner_indexes(
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

fn cancel_binding(binding: &mut LiveOrderBinding, effects: &mut Vec<ExecutionAction>) {
    if binding.status == BindingStatus::CancelPending {
        return;
    }
    binding.status = BindingStatus::CancelPending;
    if let Some(order_id) = binding.order_id.clone() {
        effects.push(ExecutionAction::CancelOrder {
            instrument: binding.request.instrument.clone(),
            order_id,
        });
    } else {
        binding.status = BindingStatus::Terminal;
    }
}

fn update_existing_binding_from_desired(binding: &mut LiveOrderBinding, desired: &DesiredBinding) {
    binding.desired_exposure = desired.desired_exposure.clone();
    binding.submit_purpose = desired.submit_purpose;
}

fn submit_desired_binding(
    state: &mut ExecutorState,
    desired: &DesiredBinding,
    effects: &mut Vec<ExecutionAction>,
) {
    let binding = desired_binding_from_spec(desired);
    let effect = ExecutionAction::SubmitOrder {
        request: binding.request.clone(),
        desired_exposure: binding.desired_exposure.clone(),
        submit_purpose: binding.submit_purpose,
        recovery_token: submit_recovery_token(&binding),
    };
    state.bindings.push(binding);
    effects.push(effect);
}

fn submit_recovery_token(binding: &LiveOrderBinding) -> SubmitRecoveryToken {
    SubmitRecoveryToken::from_binding(binding)
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
    submit_intent: &SubmitIntentInput<'_>,
) -> Option<f64> {
    let raw_price = match operation.direction {
        BoundaryDirection::Up => boundary.trigger_price,
        BoundaryDirection::Down => {
            trigger_price_for_boundary(boundary.lower_exposure.0, submit_intent.config)
        }
    };
    raw_price.is_finite().then(|| {
        round_passive_price(
            raw_price,
            submit_intent.exchange_rules.price_tick,
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

fn direction_for_gap(gap: &Exposure) -> Option<BoundaryDirection> {
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

fn next_client_order_id(policy: PolicyKind) -> String {
    let instance_id = Uuid::new_v4().simple();
    match policy {
        PolicyKind::ManualOverride => format!("bo-{instance_id}"),
        PolicyKind::Flatten => format!("bf-{instance_id}"),
        PolicyKind::CatchUp => format!("bc-{instance_id}"),
        PolicyKind::CurveMaker => format!("bk-{instance_id}"),
    }
}

fn next_binding_id(policy: PolicyKind) -> String {
    let instance_id = BINDING_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    match policy {
        PolicyKind::ManualOverride => format!("binding-manual-{instance_id}"),
        PolicyKind::Flatten => format!("binding-flatten-{instance_id}"),
        PolicyKind::CatchUp => format!("binding-catch-up-{instance_id}"),
        PolicyKind::CurveMaker => format!("binding-maker-{instance_id}"),
    }
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::ExchangeRules;
    use std::sync::LazyLock;

    use super::*;
    use crate::executor::boundary::{BoundaryId, ProfileRevision};
    use crate::executor::ledger::{BoundaryProgress, BoundaryProgressEntry};
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

    fn looks_like_u64(value: &str) -> bool {
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
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

    fn instrument() -> &'static Instrument {
        static INSTRUMENT: LazyLock<Instrument> =
            LazyLock::new(|| Instrument::new(Venue::Binance, "BTCUSDT"));
        &INSTRUMENT
    }

    fn input<'a>(
        config: &'a TrackConfig,
        rules: &'a ExchangeRules,
        current_exposure: Exposure,
        desired_exposure: Exposure,
    ) -> ExecutorInput<'a> {
        ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
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
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            None,
        )
    }

    fn input_with_gate<'a>(
        config: &'a TrackConfig,
        rules: &'a ExchangeRules,
        current_exposure: Exposure,
        desired_exposure: Exposure,
        price_execution_gate: PriceExecutionGate,
        previous_state: &'a ExecutorState,
    ) -> ExecutorInput<'a> {
        ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
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
                policy_context: PolicyContext::Normal,
                price_execution_gate,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(previous_state),
        )
    }

    #[test]
    fn catch_up_policy_submits_buy_for_due_up_operation_when_uncovered() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0)));

        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be submitted");
        let request = plan
            .effects
            .iter()
            .find_map(|effect| match effect {
                ExecutionAction::SubmitOrder { request, .. }
                    if request.client_order_id == catch_up_binding.request.client_order_id =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("catch-up submit effect should exist");
        assert_eq!(request.side, Side::Buy);
        assert_eq!(request.price, 100.1);
        assert!((request.quantity - 2.0).abs() < 1e-9);
        assert_eq!(catch_up_binding.allocations.len(), 2);
    }

    #[test]
    fn planning_replaces_changed_binding_request_in_same_round() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        let existing_binding = previous.bindings[0].clone();
        previous.bindings[0].status = BindingStatus::Working;
        previous.bindings[0].order_id = Some("existing-order".to_string());
        previous.bindings[0].request.price = 100.3;

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                desired_exposure: Exposure(2.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&previous),
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            ExecutionAction::CancelOrder { order_id, .. } if order_id == "existing-order"
        )));
        let replacement = plan
            .effects
            .iter()
            .find_map(|effect| match effect {
                ExecutionAction::SubmitOrder { request, .. }
                    if request.client_order_id != existing_binding.request.client_order_id =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("replacement submit effect should be emitted in the same round");
        assert_eq!(replacement.price, 100.1);
        assert_eq!(
            plan.state
                .bindings
                .iter()
                .filter(|binding| binding.proposal_key == existing_binding.proposal_key)
                .count(),
            2
        );
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key == existing_binding.proposal_key
                && binding.status == BindingStatus::CancelPending
        }));
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key == existing_binding.proposal_key
                && binding.status == BindingStatus::SubmitPending
                && binding.request.client_order_id == replacement.client_order_id
        }));
    }

    #[test]
    fn cancel_pending_owner_blocks_later_replacement_submit() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        previous.bindings[0].status = BindingStatus::Working;
        previous.bindings[0].order_id = Some("existing-order".to_string());
        previous.bindings[0].request.price = 100.3;

        let replacing = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                desired_exposure: Exposure(2.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&previous),
        ));

        assert!(replacing.state.bindings.iter().any(|binding| {
            binding.status == BindingStatus::CancelPending
                && binding.order_id.as_deref() == Some("existing-order")
        }));
        let replacing_catch_up_key = replacing
            .state
            .bindings
            .iter()
            .find(|binding| {
                binding.status == BindingStatus::SubmitPending
                    && binding.order_id.is_none()
                    && binding.proposal_key.policy == PolicyKind::CatchUp
            })
            .expect("replacement catch-up binding should be submit pending")
            .proposal_key
            .clone();

        let next = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                desired_exposure: Exposure(2.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 100.0,
                    best_ask: 100.2,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&replacing.state),
        ));

        assert!(
            !next
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. })),
            "cancel-pending owner must block additional replacement submits"
        );
        assert_eq!(
            next.state
                .bindings
                .iter()
                .filter(|binding| {
                    binding.status == BindingStatus::SubmitPending
                        && binding.proposal_key == replacing_catch_up_key
                })
                .count(),
            1,
            "existing downstream replacement should stay as the only submit pending"
        );
    }

    #[test]
    fn gate_cancels_existing_increase_inventory_working_order() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        previous.bindings[0].status = BindingStatus::Working;
        previous.bindings[0].order_id = Some("increase-order".to_string());

        let plan = plan(input_with_gate(
            &config,
            &rules,
            Exposure(0.0),
            Exposure(2.0),
            PriceExecutionGate::ManualRiskReductionOnly {
                reason: crate::price_gate::PriceExecutionBlockReason::MarkBookDivergence,
            },
            &previous,
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            ExecutionAction::CancelOrder { order_id, .. } if order_id == "increase-order"
        )));
        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );
        assert_eq!(plan.state.bindings[0].status, BindingStatus::CancelPending);
    }

    #[test]
    fn gate_keeps_existing_decrease_inventory_working_order_without_replacement() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(2.0), Exposure(0.0))).state;
        previous.bindings[0].status = BindingStatus::Working;
        previous.bindings[0].order_id = Some("decrease-order".to_string());
        previous.bindings[0].request.price = 99.7;

        let plan = plan(input_with_gate(
            &config,
            &rules,
            Exposure(2.0),
            Exposure(0.0),
            PriceExecutionGate::ManualRiskReductionOnly {
                reason: crate::price_gate::PriceExecutionBlockReason::MarkBookDivergence,
            },
            &previous,
        ));

        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::CancelOrder { .. }))
        );
        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );
        assert_eq!(plan.state.bindings[0].status, BindingStatus::Working);
        assert_eq!(plan.state.bindings[0].request.price, 99.7);
    }

    #[test]
    fn gate_does_not_mutate_working_order_when_ledger_invariant_fails() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        previous.bindings[0].status = BindingStatus::Working;
        previous.bindings[0].order_id = Some("increase-order".to_string());
        previous.ledger_state.progress.push(BoundaryProgressEntry {
            boundary_id: BoundaryId {
                profile_revision: ProfileRevision(previous.ledger_state.profile_revision.0.clone()),
                lower_exposure_bp: 0,
                upper_exposure_bp: 10_000,
            },
            progress: BoundaryProgress {
                cumulative_up: 1.2,
                cumulative_down: 0.0,
            },
        });

        let plan = plan(input_with_gate(
            &config,
            &rules,
            Exposure(1.2),
            Exposure(1.0),
            PriceExecutionGate::ManualRiskReductionOnly {
                reason: crate::price_gate::PriceExecutionBlockReason::MarkBookDivergence,
            },
            &previous,
        ));

        assert_eq!(
            plan.state.recovery_anomaly,
            Some(crate::executor::RecoveryAnomaly::BoundaryProgressOutOfRange)
        );
        assert_eq!(plan.effects, vec![ExecutionAction::NoOp]);
        assert_eq!(plan.state.bindings[0].status, BindingStatus::Working);
    }

    #[test]
    fn gate_keeps_existing_short_reduce_only_working_order() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(-2.0), Exposure(0.0))).state;
        previous.bindings[0].status = BindingStatus::Working;
        previous.bindings[0].order_id = Some("short-reduce-order".to_string());
        assert!(previous.bindings[0].request.reduce_only);

        let plan = plan(input_with_gate(
            &config,
            &rules,
            Exposure(-2.0),
            Exposure(0.0),
            PriceExecutionGate::ManualRiskReductionOnly {
                reason: crate::price_gate::PriceExecutionBlockReason::MarkBookDivergence,
            },
            &previous,
        ));

        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::CancelOrder { .. }))
        );
        assert_eq!(plan.state.bindings[0].status, BindingStatus::Working);
    }

    #[test]
    fn catch_up_policy_takes_over_due_curve_maker_in_same_round() {
        let config = config();
        let rules = rules();
        let initial = plan(input(&config, &rules, Exposure(0.0), Exposure(0.0))).state;
        let mut previous = initial.clone();
        let maker = previous
            .bindings
            .iter_mut()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
            .expect("maker binding should exist");
        maker.status = BindingStatus::Working;
        maker.order_id = Some("maker-order".to_string());
        maker.policy_state = BindingPolicyState::CurveMaker {
            due_grace_started_at: Some(Utc::now() - chrono::Duration::milliseconds(60_001)),
        };

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                desired_exposure: Exposure(1.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&previous),
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            ExecutionAction::CancelOrder { order_id, .. } if order_id == "maker-order"
        )));
        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            ExecutionAction::SubmitOrder { request, .. } if request.side == Side::Buy
        )));
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CurveMaker
                && binding.status == BindingStatus::CancelPending
        }));
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CatchUp
                && binding.status == BindingStatus::SubmitPending
        }));
    }

    #[test]
    fn planning_removes_terminal_bindings_before_returning_state() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(1.0))).state;
        previous.bindings[0].status = BindingStatus::Terminal;

        let next = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                desired_exposure: Exposure(0.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&previous),
        ));

        assert!(
            next.state
                .bindings
                .iter()
                .all(|binding| binding.status != BindingStatus::Terminal)
        );
    }

    #[test]
    fn curve_maker_policy_emits_sell_side_reduce_only_binding() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(1.0), Exposure(1.0)));

        let sell_maker = plan
            .state
            .bindings
            .iter()
            .find(|binding| {
                binding.proposal_key.policy == PolicyKind::CurveMaker
                    && binding.request.side == Side::Sell
                    && binding.request.reduce_only
            })
            .expect("sell-side reduce-only maker binding should be planned");
        assert_eq!(sell_maker.request.price, 100.0);
        assert_eq!(sell_maker.allocations.len(), 1);
    }

    #[test]
    fn catch_up_takes_over_far_due_curve_maker_at_threshold() {
        let config = config();
        let rules = rules();
        let initial = plan(input(&config, &rules, Exposure(0.0), Exposure(0.0))).state;
        let mut previous = initial.clone();
        let maker = previous
            .bindings
            .iter_mut()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
            .expect("maker binding should exist");
        maker.status = BindingStatus::Working;
        maker.order_id = Some("maker-order".to_string());
        maker.policy_state = BindingPolicyState::CurveMaker {
            due_grace_started_at: None,
        };

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                desired_exposure: Exposure(1.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&previous),
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            ExecutionAction::CancelOrder { order_id, .. } if order_id == "maker-order"
        )));
        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            ExecutionAction::SubmitOrder { request, .. } if request.side == Side::Buy
        )));
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CurveMaker
                && binding.status == BindingStatus::CancelPending
        }));
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CatchUp
                && binding.status == BindingStatus::SubmitPending
        }));
    }

    #[test]
    fn catch_up_keeps_nearby_working_curve_maker_that_can_cover_gap() {
        let config = config();
        let rules = rules();
        let initial = plan(input(&config, &rules, Exposure(0.0), Exposure(0.0))).state;
        let mut previous = initial.clone();
        let maker = previous
            .bindings
            .iter_mut()
            .find(|binding| {
                binding.proposal_key.policy == PolicyKind::CurveMaker
                    && binding.request.side == Side::Buy
            })
            .expect("buy maker binding should exist");
        maker.status = BindingStatus::Working;
        maker.order_id = Some("near-maker-order".to_string());
        maker.request.price = 100.1;
        maker.policy_state = BindingPolicyState::CurveMaker {
            due_grace_started_at: None,
        };

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                desired_exposure: Exposure(1.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&previous),
        ));

        assert!(
            !plan.effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::CancelOrder { order_id, .. } if order_id == "near-maker-order"))
        );
        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { request, .. } if request.client_order_id.starts_with("bc-")))
        );
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.order_id.as_deref() == Some("near-maker-order")
                && binding.status == BindingStatus::Working
        }));
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

    #[test]
    fn planning_uses_process_local_binding_ids_and_exchange_safe_client_order_ids() {
        let binding_id = next_binding_id(PolicyKind::CatchUp);
        let binding_suffix = binding_id
            .strip_prefix("binding-catch-up-")
            .expect("binding id should keep the catch-up prefix");
        assert!(looks_like_u64(binding_suffix));

        for (policy, expected_prefix) in [
            (PolicyKind::ManualOverride, "bo-"),
            (PolicyKind::Flatten, "bf-"),
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

    #[test]
    fn planning_enters_expected_exposure_mismatch_anomaly_when_ledger_drift_is_unexplained() {
        let config = config();
        let rules = rules();
        let mut state = ExecutorState::empty(Utc::now()).ensure_revision(&config, Exposure(0.0));
        state.ledger_state.progress.push(BoundaryProgressEntry {
            boundary_id: BoundaryId {
                profile_revision: ProfileRevision(state.ledger_state.profile_revision.0.clone()),
                lower_exposure_bp: 0,
                upper_exposure_bp: 10_000,
            },
            progress: BoundaryProgress {
                cumulative_up: 1.0,
                cumulative_down: 0.0,
            },
        });

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
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
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&state),
        ));

        assert_eq!(
            plan.state.recovery_anomaly,
            Some(crate::executor::RecoveryAnomaly::ExpectedExposureMismatch)
        );
        assert_eq!(plan.effects, vec![ExecutionAction::NoOp]);
    }

    #[test]
    fn planning_allows_expected_exposure_drift_within_active_binding_budget() {
        let config = config();
        let rules = rules();
        let previous = plan(input(&config, &rules, Exposure(0.0), Exposure(1.0))).state;

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.6),
                desired_exposure: Exposure(1.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&previous),
        ));

        assert_eq!(plan.state.recovery_anomaly, None);
    }

    #[test]
    fn planning_reports_boundary_progress_out_of_range_before_using_ledger_view() {
        let config = config();
        let rules = rules();
        let mut state = ExecutorState::empty(Utc::now()).ensure_revision(&config, Exposure(0.0));
        state.ledger_state.progress.push(BoundaryProgressEntry {
            boundary_id: BoundaryId {
                profile_revision: ProfileRevision(state.ledger_state.profile_revision.0.clone()),
                lower_exposure_bp: 0,
                upper_exposure_bp: 10_000,
            },
            progress: BoundaryProgress {
                cumulative_up: 1.2,
                cumulative_down: 0.0,
            },
        });

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                exchange_rules: &rules,
                base_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(1.2),
                desired_exposure: Exposure(1.0),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 99.9,
                    best_ask: 100.1,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&state),
        ));

        assert_eq!(
            plan.state.recovery_anomaly,
            Some(crate::executor::RecoveryAnomaly::BoundaryProgressOutOfRange)
        );
        assert_eq!(plan.effects, vec![ExecutionAction::NoOp]);
    }
}
