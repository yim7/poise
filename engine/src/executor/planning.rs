use chrono::{DateTime, Utc};
use poise_core::instrument::InstrumentSpec;
use poise_core::strategy::TrackConfig;
use poise_core::types::Exposure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

use crate::execution_gate::ExecutionGateDecision;
use crate::execution_plan::{DecisionReason, TrackEffect};
use crate::ports::{ExecutionQuote, OrderRequest};
use crate::price_gate::{
    PriceExecutionGate, SubmitPurpose, WorkingOrderGateAction, allows_submit,
    working_order_gate_action,
};
use crate::runtime::ExecutorState;
use poise_core::track::Instrument;

use super::binding::{
    BindingPolicyState, BindingProposal, BindingStatus, LiveOrderBinding, SubmitRecoveryToken,
    active_binding_exposure_budget,
};
use super::boundary::{discretize_boundaries, profile_revision_for_config};
use super::ledger::BoundaryLedgerView;
use super::policy::{
    BindingReconciliationDecision, DesiredBinding, PolicyContext, PolicyKind, PolicyPlanningInput,
    classify_binding_reconciliation, plan_policy_bindings,
};
use super::recovery::RecoveryAnomaly;

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
    pub instrument_spec: &'a InstrumentSpec,
    pub position_qty_per_unit: f64,
    pub min_rebalance_units: f64,
    pub current_exposure: Exposure,
    // Risk-approved target retained for risk-reducing passive maker selection.
    pub risk_approved_target: Exposure,
    // Final target already resolved by the reconciler; active policies must not derive it again.
    pub execution_target: Exposure,
    // Execution constraints stay separate from target ownership and order maintenance.
    pub execution_gate_decision: ExecutionGateDecision,
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
    pub effects: Vec<TrackEffect>,
    pub reason: Option<DecisionReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingSubmitHint {
    pub request: OrderRequest,
    pub desired_exposure: Exposure,
    pub submit_purpose: SubmitPurpose,
    pub recovery_token: SubmitRecoveryToken,
    pub phase: PendingSubmitPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSubmitPhase {
    DispatchInFlight,
    AwaitingExchangeState,
}

const CURVE_MAKER_LEVELS_PER_SIDE: usize = 3;
const CURVE_MAKER_GRACE_MS: i64 = 60_000;

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
        return recovery_required_plan(state);
    }

    let boundaries = discretize_boundaries(
        submit_intent.config,
        profile_revision_for_config(submit_intent.config),
    );
    let boundary_epsilon = boundary_exposure_epsilon(&submit_intent);
    let position_epsilon = position_exposure_epsilon(&submit_intent);
    let execution_target = submit_intent.execution_target.clone();
    let execution_view = match BoundaryLedgerView::try_from_boundaries(
        &boundaries,
        &state.ledger_state,
        execution_target.clone(),
        boundary_epsilon,
    ) {
        Ok(view) => view,
        Err(_error) => {
            state.recovery_anomaly = Some(RecoveryAnomaly::BoundaryProgressOutOfRange);
            return recovery_required_plan(state);
        }
    };
    let desired_view = match BoundaryLedgerView::try_from_boundaries(
        &boundaries,
        &state.ledger_state,
        submit_intent.risk_approved_target.clone(),
        boundary_epsilon,
    ) {
        Ok(view) => view,
        Err(_error) => {
            state.recovery_anomaly = Some(RecoveryAnomaly::BoundaryProgressOutOfRange);
            return recovery_required_plan(state);
        }
    };
    if state.ledger_state.has_unexplained_exposure_drift(
        &submit_intent.current_exposure,
        active_binding_exposure_budget(&state.bindings),
        position_epsilon,
    ) {
        state.recovery_anomaly = Some(RecoveryAnomaly::ExpectedExposureMismatch);
        return recovery_required_plan(state);
    }

    let mut effects =
        apply_price_gate_to_existing_bindings(&mut state, submit_intent.price_execution_gate);
    effects.extend(apply_execution_gate_to_existing_bindings(
        &mut state,
        &submit_intent.execution_gate_decision,
    ));

    if matches!(
        &submit_intent.execution_gate_decision,
        ExecutionGateDecision::NoSubmit { .. }
    ) {
        let reason = maintenance_reason(&effects, &state);
        return finish_plan(state, Vec::new(), effects, reason);
    }

    if !allows_submit(
        submit_intent.price_execution_gate,
        submit_intent.submit_purpose,
    ) {
        let has_target_gap = !submit_intent
            .current_exposure
            .delta(&submit_intent.execution_target)
            .is_zero();
        let reason = if has_target_gap || !effects.is_empty() {
            Some(DecisionReason::PriceGateBlocked)
        } else {
            maintenance_reason(&effects, &state)
        };
        return finish_plan(state, Vec::new(), effects, reason);
    }

    if submit_intent.policy_context == PolicyContext::Normal {
        // This only starts or clears the due grace timer. Reconciliation owns
        // the later choice to keep, cancel, or replace an existing maker.
        refresh_curve_maker_due_grace_start(&mut state, &desired_view, submit_intent.observed_at);
    }
    let policy_input = PolicyPlanningInput {
        execution_view: &execution_view,
        desired_view: &desired_view,
        boundaries: &boundaries,
        instrument: submit_intent.instrument,
        config: submit_intent.config,
        instrument_spec: submit_intent.instrument_spec,
        position_qty_per_unit: submit_intent.position_qty_per_unit,
        min_rebalance_units: submit_intent.min_rebalance_units,
        current_exposure: &submit_intent.current_exposure,
        execution_target: &execution_target,
        risk_approved_target: &submit_intent.risk_approved_target,
        execution_quote: submit_intent.execution_quote,
        submit_purpose: submit_intent.submit_purpose,
        boundary_epsilon,
        curve_maker_levels_per_side: CURVE_MAKER_LEVELS_PER_SIDE,
    };

    let policy_plan = plan_policy_bindings(submit_intent.policy_context, &policy_input);
    let desired_bindings = policy_plan.desired_bindings;
    effects.extend(reconcile_bindings(
        &mut state,
        &desired_bindings,
        submit_intent.instrument_spec,
        submit_intent.observed_at,
        CURVE_MAKER_GRACE_MS,
    ));

    let reason = execution_reason(&effects, &state, &desired_bindings, policy_plan.reason);
    finish_plan(
        state,
        desired_bindings
            .iter()
            .map(|binding| binding.proposal.clone())
            .collect(),
        effects,
        reason,
    )
}

pub fn maintain_protected_bindings(input: ExecutorInput<'_>) -> ExecutorPlan {
    let ExecutorInput {
        submit_intent,
        executor_state,
    } = input;
    let mut state = executor_state
        .cloned()
        .unwrap_or_else(|| ExecutorState::empty(submit_intent.observed_at))
        .ensure_revision(submit_intent.config, submit_intent.current_exposure.clone());

    if state.recovery_anomaly.is_some() {
        return recovery_required_plan(state);
    }

    let position_epsilon = position_exposure_epsilon(&submit_intent);
    if state.ledger_state.has_unexplained_exposure_drift(
        &submit_intent.current_exposure,
        active_binding_exposure_budget(&state.bindings),
        position_epsilon,
    ) {
        state.recovery_anomaly = Some(RecoveryAnomaly::ExpectedExposureMismatch);
        return recovery_required_plan(state);
    }

    let mut effects =
        apply_price_gate_to_existing_bindings(&mut state, submit_intent.price_execution_gate);
    effects.extend(cancel_inventory_increasing_bindings(&mut state));
    let has_target_gap = !submit_intent
        .current_exposure
        .delta(&submit_intent.execution_target)
        .is_zero();
    let reason = if !allows_submit(
        submit_intent.price_execution_gate,
        submit_intent.submit_purpose,
    ) && (has_target_gap || !effects.is_empty())
    {
        Some(DecisionReason::PriceGateBlocked)
    } else {
        maintenance_reason(&effects, &state)
    };
    finish_plan(state, Vec::new(), effects, reason)
}

pub fn refresh_state(
    previous_state: &ExecutorState,
    config: &TrackConfig,
    current_exposure: &Exposure,
) -> ExecutorState {
    previous_state.ensure_revision(config, current_exposure.clone())
}

fn recovery_required_plan(state: ExecutorState) -> ExecutorPlan {
    finish_plan(
        state,
        Vec::new(),
        Vec::new(),
        Some(DecisionReason::RecoveryRequired),
    )
}

fn finish_plan(
    mut state: ExecutorState,
    desired_bindings: Vec<BindingProposal>,
    effects: Vec<TrackEffect>,
    reason: Option<DecisionReason>,
) -> ExecutorPlan {
    // Terminal bindings are an ExecutorPlan output invariant, including early returns.
    state
        .bindings
        .retain(|binding| binding.status != BindingStatus::Terminal);

    ExecutorPlan {
        state,
        desired_bindings,
        effects,
        reason,
    }
}

fn maintenance_reason(effects: &[TrackEffect], state: &ExecutorState) -> Option<DecisionReason> {
    if effects.iter().any(|effect| {
        matches!(
            effect,
            TrackEffect::CancelOrder { .. } | TrackEffect::CancelAll { .. }
        )
    }) || state.bindings.iter().any(|binding| {
        matches!(
            binding.status,
            BindingStatus::CancelAfterSubmit | BindingStatus::CancelPending
        )
    }) {
        Some(DecisionReason::WaitingCancel)
    } else {
        None
    }
}

fn execution_reason(
    effects: &[TrackEffect],
    state: &ExecutorState,
    desired_bindings: &[DesiredBinding],
    policy_reason: Option<DecisionReason>,
) -> Option<DecisionReason> {
    if effects
        .iter()
        .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. }))
    {
        return Some(DecisionReason::SubmitPlanned);
    }
    if let Some(reason) = maintenance_reason(effects, state) {
        return Some(reason);
    }
    if !desired_bindings.is_empty() {
        if state.bindings.iter().any(|binding| {
            matches!(
                binding.status,
                BindingStatus::SubmitPending | BindingStatus::SubmitAuthorized
            )
        }) {
            return Some(DecisionReason::SubmitPlanned);
        }
        return Some(DecisionReason::CoveredByWorkingOrder);
    }
    policy_reason
}

fn refresh_curve_maker_due_grace_start(
    state: &mut ExecutorState,
    view: &BoundaryLedgerView,
    observed_at: DateTime<Utc>,
) {
    for binding in &mut state.bindings {
        if binding.proposal_key.policy != PolicyKind::CurveMaker
            || !matches!(
                binding.status,
                BindingStatus::SubmitPending
                    | BindingStatus::SubmitAuthorized
                    | BindingStatus::Working
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

        due_grace_started_at.get_or_insert(observed_at);
    }
}

fn apply_price_gate_to_existing_bindings(
    state: &mut ExecutorState,
    gate: PriceExecutionGate,
) -> Vec<TrackEffect> {
    let mut effects = Vec::new();
    for binding in &mut state.bindings {
        if !matches!(
            binding.status,
            BindingStatus::SubmitPending | BindingStatus::SubmitAuthorized | BindingStatus::Working
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

fn apply_execution_gate_to_existing_bindings(
    state: &mut ExecutorState,
    decision: &ExecutionGateDecision,
) -> Vec<TrackEffect> {
    if matches!(decision, ExecutionGateDecision::Open) {
        return Vec::new();
    }

    cancel_inventory_increasing_bindings(state)
}

fn cancel_inventory_increasing_bindings(state: &mut ExecutorState) -> Vec<TrackEffect> {
    let mut effects = Vec::new();
    for binding in &mut state.bindings {
        if !matches!(
            binding.status,
            BindingStatus::SubmitPending | BindingStatus::SubmitAuthorized | BindingStatus::Working
        ) {
            continue;
        }
        if order_role_for_binding(binding) == OrderRole::IncreaseInventory {
            cancel_binding(binding, &mut effects);
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

fn reconcile_bindings(
    state: &mut ExecutorState,
    desired_bindings: &[DesiredBinding],
    instrument_spec: &InstrumentSpec,
    observed_at: DateTime<Utc>,
    curve_maker_grace_ms: i64,
) -> Vec<TrackEffect> {
    let mut effects = Vec::new();
    let existing_count = state.bindings.len();
    let mut matched_existing = BTreeSet::new();

    for desired in desired_bindings {
        match classify_binding_reconciliation(
            &state.bindings[..existing_count],
            &state.bindings,
            desired,
            instrument_spec,
            observed_at,
            curve_maker_grace_ms,
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
            BindingReconciliationDecision::CancelActiveOwners { indexes } => {
                for index in indexes {
                    matched_existing.insert(index);
                    cancel_binding(&mut state.bindings[index], &mut effects);
                }
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

fn binding_is_active(binding: &LiveOrderBinding) -> bool {
    super::binding::binding_is_active(binding)
}

fn cancel_binding(binding: &mut LiveOrderBinding, effects: &mut Vec<TrackEffect>) {
    match binding.status {
        BindingStatus::SubmitPending => {
            binding.status = BindingStatus::Terminal;
        }
        BindingStatus::SubmitAuthorized => {
            binding.status = BindingStatus::CancelAfterSubmit;
        }
        BindingStatus::Working => {
            let Some(order_id) = binding.order_id.clone() else {
                binding.status = BindingStatus::Terminal;
                return;
            };
            binding.status = BindingStatus::CancelPending;
            effects.push(TrackEffect::CancelOrder {
                instrument: binding.request.instrument.clone(),
                order_id,
            });
        }
        BindingStatus::CancelAfterSubmit => {
            let Some(order_id) = binding.order_id.clone() else {
                return;
            };
            binding.status = BindingStatus::CancelPending;
            effects.push(TrackEffect::CancelOrder {
                instrument: binding.request.instrument.clone(),
                order_id,
            });
        }
        BindingStatus::CancelPending | BindingStatus::Terminal => {}
    }
}

fn update_existing_binding_from_desired(binding: &mut LiveOrderBinding, desired: &DesiredBinding) {
    binding.desired_exposure = desired.desired_exposure.clone();
    binding.submit_purpose = desired.submit_purpose;
}

fn submit_desired_binding(
    state: &mut ExecutorState,
    desired: &DesiredBinding,
    effects: &mut Vec<TrackEffect>,
) {
    let binding = desired_binding_from_spec(desired);
    let effect = TrackEffect::SubmitOrder {
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

fn boundary_exposure_epsilon(input: &SubmitIntentInput<'_>) -> f64 {
    (input.min_rebalance_units * 0.01).max(f64::EPSILON)
}

fn position_exposure_epsilon(input: &SubmitIntentInput<'_>) -> f64 {
    let quantity_step_as_exposure = if input.position_qty_per_unit <= f64::EPSILON {
        0.0
    } else {
        input.instrument_spec.position_qty_step().value() / input.position_qty_per_unit
    };
    boundary_exposure_epsilon(input).max(quantity_step_as_exposure)
}

fn next_binding_id(policy: PolicyKind) -> String {
    let instance_id = Uuid::new_v4().simple();
    match policy {
        PolicyKind::ManualOverride => format!("binding-manual-{instance_id}"),
        PolicyKind::ReduceOnly => format!("binding-reduce-only-{instance_id}"),
        PolicyKind::CatchUp => format!("binding-catch-up-{instance_id}"),
        PolicyKind::CurveMaker => format!("binding-maker-{instance_id}"),
    }
}

#[cfg(test)]
mod tests {
    use poise_core::instrument::InstrumentSpec;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::Side;
    use std::collections::BTreeSet;
    use std::sync::LazyLock;

    use super::*;
    use crate::executor::boundary::{BoundaryId, ProfileRevision};
    use crate::executor::ledger::BoundaryProgress;
    use crate::ports::ExecutionQuote;
    use crate::price_gate::PriceExecutionGate;
    use poise_core::track::{Instrument, Venue};

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
            risk_acquisition: Default::default(),
        }
    }

    fn rules() -> InstrumentSpec {
        crate::test_instrument::linear_spec(0.1, 0.01, 0.0, 0.0)
    }

    fn instrument() -> &'static Instrument {
        static INSTRUMENT: LazyLock<Instrument> =
            LazyLock::new(|| Instrument::new(Venue::Binance, "BTCUSDT"));
        &INSTRUMENT
    }

    fn input<'a>(
        config: &'a TrackConfig,
        rules: &'a InstrumentSpec,
        current_exposure: Exposure,
        desired_exposure: Exposure,
    ) -> ExecutorInput<'a> {
        ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config,
                instrument_spec: rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure,
                execution_target: desired_exposure.clone(),
                execution_gate_decision: Default::default(),
                risk_approved_target: desired_exposure,
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

    fn input_with_position_qty_per_unit<'a>(
        config: &'a TrackConfig,
        rules: &'a InstrumentSpec,
        position_qty_per_unit: f64,
        current_exposure: Exposure,
        desired_exposure: Exposure,
    ) -> ExecutorInput<'a> {
        let mut input = input(config, rules, current_exposure, desired_exposure);
        input.submit_intent.position_qty_per_unit = position_qty_per_unit;
        input
    }

    fn input_with_execution_target<'a>(
        config: &'a TrackConfig,
        rules: &'a InstrumentSpec,
        current_exposure: Exposure,
        desired_exposure: Exposure,
        execution_target: Exposure,
    ) -> ExecutorInput<'a> {
        let mut input = input(config, rules, current_exposure, desired_exposure);
        input.submit_intent.execution_target = execution_target;
        input
    }

    fn input_with_gate<'a>(
        config: &'a TrackConfig,
        rules: &'a InstrumentSpec,
        current_exposure: Exposure,
        desired_exposure: Exposure,
        price_execution_gate: PriceExecutionGate,
        previous_state: &'a ExecutorState,
    ) -> ExecutorInput<'a> {
        ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config,
                instrument_spec: rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure,
                execution_target: desired_exposure.clone(),
                execution_gate_decision: Default::default(),
                risk_approved_target: desired_exposure,
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

    fn input_with_execution_gate<'a>(
        config: &'a TrackConfig,
        rules: &'a InstrumentSpec,
        current_exposure: Exposure,
        desired_exposure: Exposure,
        execution_gate_decision: ExecutionGateDecision,
        previous_state: &'a ExecutorState,
    ) -> ExecutorInput<'a> {
        let mut input = input(config, rules, current_exposure, desired_exposure);
        input.submit_intent.execution_gate_decision = execution_gate_decision;
        input.executor_state = Some(previous_state);
        input
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
                TrackEffect::SubmitOrder { request, .. }
                    if request.client_order_id == catch_up_binding.request.client_order_id =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("catch-up submit effect should exist");
        assert_eq!(request.side, Side::Buy);
        assert_eq!(request.price, 100.1);
        assert!((request.quantity - 1.0).abs() < 1e-9);
        assert_eq!(catch_up_binding.allocations.len(), 1);
        assert_eq!(plan.reason, Some(DecisionReason::SubmitPlanned));
    }

    #[test]
    fn planning_explains_threshold_venue_and_existing_order_holds() {
        let config = config();
        let rules = rules();

        let at_target = plan(input(&config, &rules, Exposure(0.0), Exposure(0.0)));
        assert!(at_target.effects.is_empty());
        assert_eq!(at_target.reason, None);

        let below_rebalance = plan(input(&config, &rules, Exposure(0.0), Exposure(0.5)));
        assert!(below_rebalance.effects.is_empty());
        assert_eq!(below_rebalance.reason, Some(DecisionReason::BelowRebalance));

        let restrictive_rules = crate::test_instrument::linear_spec(0.1, 0.01, 10.0, 0.0);
        let below_venue = plan(input(
            &config,
            &restrictive_rules,
            Exposure(0.0),
            Exposure(2.0),
        ));
        assert!(below_venue.effects.is_empty());
        assert_eq!(below_venue.reason, Some(DecisionReason::BelowVenueMinimum));

        let mut owned = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        for binding in &mut owned.bindings {
            binding.status = BindingStatus::Working;
            binding.order_id = Some("working-order".to_string());
        }
        let covered = plan(ExecutorInput::new(
            input(&config, &rules, Exposure(0.0), Exposure(2.0)).submit_intent,
            Some(&owned),
        ));
        assert!(covered.effects.is_empty());
        assert_eq!(covered.reason, Some(DecisionReason::CoveredByWorkingOrder));
    }

    #[test]
    fn catch_up_policy_uses_position_qty_per_exposure_unit() {
        let config = config();
        let rules = rules();

        let plan = plan(input_with_position_qty_per_unit(
            &config,
            &rules,
            10.0,
            Exposure(0.0),
            Exposure(2.0),
        ));

        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be submitted");
        assert!((catch_up_binding.request.quantity - 10.0).abs() < 1e-9);
    }

    #[test]
    fn catch_up_policy_aggregates_due_operations_into_single_binding() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(0.0), Exposure(3.0)));

        let catch_up_bindings = plan
            .state
            .bindings
            .iter()
            .filter(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .collect::<Vec<_>>();
        assert_eq!(catch_up_bindings.len(), 1);
        assert_eq!(catch_up_bindings[0].allocations.len(), 1);
        assert!((catch_up_bindings[0].request.quantity - 1.0).abs() < 1e-9);
        assert_eq!(
            plan.effects
                .iter()
                .filter(|effect| matches!(effect, TrackEffect::SubmitOrder { request, .. } if request.client_order_id.starts_with("bc")))
                .count(),
            1
        );
    }

    #[test]
    fn catch_up_follows_final_execution_target_instead_of_risk_approved_target() {
        let config = config();
        let rules = rules();

        let plan = plan(input_with_execution_target(
            &config,
            &rules,
            Exposure(0.0),
            Exposure(-4.0),
            Exposure(-2.0),
        ));

        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be planned toward the final execution target");

        assert_eq!(catch_up_binding.request.side, Side::Sell);
        assert!(!catch_up_binding.request.reduce_only);
        assert_eq!(catch_up_binding.desired_exposure, Exposure(-2.0));
        assert!((catch_up_binding.request.quantity - 1.0).abs() < 1e-9);
    }

    #[test]
    fn final_execution_target_does_not_make_catch_up_reduce_advantage_position() {
        let config = config();
        let rules = rules();

        let plan = plan(input_with_execution_target(
            &config,
            &rules,
            Exposure(-3.0),
            Exposure(-4.0),
            Exposure(-3.0),
        ));

        assert!(
            !plan
                .state
                .bindings
                .iter()
                .any(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
        );
    }

    #[test]
    fn risk_approved_target_still_allows_reduce_only_maker_when_final_target_holds() {
        let config = config();
        let rules = rules();

        let plan = plan(input_with_execution_target(
            &config,
            &rules,
            Exposure(1.0),
            Exposure(5.0),
            Exposure(1.0),
        ));

        assert!(
            !plan
                .state
                .bindings
                .iter()
                .any(|binding| { binding.proposal_key.policy == PolicyKind::CatchUp })
        );
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CurveMaker
                && binding.request.side == Side::Sell
                && binding.request.reduce_only
        }));
    }

    #[test]
    fn risk_reduction_follows_final_execution_target_without_frontier_input() {
        let config = config();
        let rules = rules();

        let plan = plan(input_with_execution_target(
            &config,
            &rules,
            Exposure(-5.0),
            Exposure(-4.0),
            Exposure(-4.0),
        ));

        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should reduce back to desired exposure");

        assert_eq!(catch_up_binding.request.side, Side::Buy);
        assert!(catch_up_binding.request.reduce_only);
        assert_eq!(catch_up_binding.desired_exposure, Exposure(-4.0));
        assert!((catch_up_binding.request.quantity - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cross_zero_catch_up_stops_at_final_execution_target() {
        let config = config();
        let rules = rules();

        let plan = plan(input_with_execution_target(
            &config,
            &rules,
            Exposure(-0.25),
            Exposure(2.0),
            Exposure(1.0),
        ));

        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should cross the residual short to the released long target");

        assert_eq!(catch_up_binding.request.side, Side::Buy);
        assert!(!catch_up_binding.request.reduce_only);
        assert_eq!(catch_up_binding.desired_exposure, Exposure(1.0));
        assert!((catch_up_binding.request.quantity - 1.25).abs() < 1e-9);
    }

    #[test]
    fn cross_zero_catch_up_does_not_infer_a_target_beyond_final_input() {
        let config = config();
        let rules = rules();

        let plan = plan(input_with_execution_target(
            &config,
            &rules,
            Exposure(-1.0),
            Exposure(2.0),
            Exposure(0.5),
        ));

        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should cross the residual short to the released long target");

        assert_eq!(catch_up_binding.request.side, Side::Buy);
        assert!(!catch_up_binding.request.reduce_only);
        assert_eq!(catch_up_binding.desired_exposure, Exposure(0.5));
        assert!(
            (catch_up_binding.request.quantity - 1.5).abs() < 1e-9,
            "quantity was {}",
            catch_up_binding.request.quantity
        );
    }

    #[test]
    fn final_zero_target_flattens_sub_min_opposite_residual() {
        let config = TrackConfig {
            lower_price: 59_000.0,
            upper_price: 73_000.0,
            long_exposure_units: 5.0,
            short_exposure_units: 10.0,
            notional_per_unit: 200.0,
            min_rebalance_units: 0.3,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = crate::test_instrument::linear_spec(0.5, 0.00001, 0.00001, 5.0);

        let mut input = input_with_position_qty_per_unit(
            &config,
            &rules,
            crate::runtime::position_qty_per_exposure_unit(&config, &rules),
            Exposure(-0.1056),
            Exposure(0.4172),
        );
        input.submit_intent.execution_target = Exposure(0.0);
        input.submit_intent.execution_quote = Some(ExecutionQuote {
            best_bid: 64_362.0,
            best_ask: 64_363.0,
        });

        let plan = plan(input);

        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should flatten the sub-min residual short");

        assert_eq!(catch_up_binding.request.side, Side::Buy);
        assert!(catch_up_binding.request.reduce_only);
        assert_eq!(catch_up_binding.desired_exposure, Exposure(0.0));
        assert!((catch_up_binding.request.quantity - 0.00032).abs() < 1e-9);
    }

    #[test]
    fn catch_up_reduce_only_tracks_full_inventory_gap() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(-6.0), Exposure(-1.0)));
        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be planned");

        assert_eq!(catch_up_binding.request.side, Side::Buy);
        assert!(catch_up_binding.request.reduce_only);
        assert!((catch_up_binding.request.quantity - 5.0).abs() < 1e-9);
        assert_eq!(catch_up_binding.allocations.len(), 5);
    }

    #[test]
    fn catch_up_reduce_only_aggregates_small_exposure_steps_into_one_order() {
        let config = TrackConfig {
            lower_price: 1000.0,
            upper_price: 2000.0,
            long_exposure_units: 10.0,
            short_exposure_units: 0.0,
            notional_per_unit: 20.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = crate::test_instrument::linear_spec(0.1, 0.01, 0.01, 0.0);

        let plan = plan(input_with_position_qty_per_unit(
            &config,
            &rules,
            crate::runtime::position_qty_per_exposure_unit(&config, &rules),
            Exposure(9.75),
            Exposure(3.75),
        ));
        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should aggregate enough small exposure steps");

        assert_eq!(catch_up_binding.request.side, Side::Sell);
        assert!(catch_up_binding.request.reduce_only);
        assert!((catch_up_binding.request.quantity - 0.07).abs() < 1e-9);
        assert_eq!(catch_up_binding.allocations.len(), 11);
        let allocated_exposure = catch_up_binding
            .allocations
            .iter()
            .map(|allocation| allocation.exposure_qty)
            .sum::<f64>();
        let position_qty_per_unit = crate::runtime::position_qty_per_exposure_unit(&config, &rules);
        assert!(
            (allocated_exposure - catch_up_binding.request.quantity / position_qty_per_unit).abs()
                < 1e-9
        );
    }

    #[test]
    fn catch_up_increase_uses_minimum_executable_size_when_pace_is_too_small() {
        let config = TrackConfig {
            lower_price: 1000.0,
            upper_price: 2000.0,
            long_exposure_units: 10.0,
            short_exposure_units: 0.0,
            notional_per_unit: 20.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        };
        let rules = crate::test_instrument::linear_spec(0.1, 0.01, 0.01, 0.0);

        let plan = plan(input_with_position_qty_per_unit(
            &config,
            &rules,
            crate::runtime::position_qty_per_exposure_unit(&config, &rules),
            Exposure(0.75),
            Exposure(1.8705),
        ));
        let catch_up_binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be large enough to meet exchange quantity rules");

        assert_eq!(catch_up_binding.request.side, Side::Buy);
        assert!(!catch_up_binding.request.reduce_only);
        assert!((catch_up_binding.request.quantity - 0.01).abs() < 1e-9);
        let allocated_exposure = catch_up_binding
            .allocations
            .iter()
            .map(|allocation| allocation.exposure_qty)
            .sum::<f64>();
        assert!(
            allocated_exposure <= 1.8705 - 0.75 + 1e-9,
            "catch-up must not cross the execution target"
        );
    }

    #[test]
    fn submit_pending_catch_up_blocks_replacement_until_receipt_arrives() {
        let config = config();
        let rules = rules();
        let previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        let pending_client_order_id = previous
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be submit pending")
            .request
            .client_order_id
            .clone();

        let next = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(2.0),
                execution_target: Exposure(2.0),
                execution_gate_decision: Default::default(),
                execution_quote: Some(ExecutionQuote {
                    best_bid: 100.0,
                    best_ask: 100.2,
                }),
                policy_context: PolicyContext::Normal,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: Utc::now(),
            },
            Some(&previous),
        ));

        assert!(
            !next
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. })),
            "a submit-pending catch-up must hold the operation while its exchange receipt is unknown"
        );
        assert!(next.state.bindings.iter().any(|binding| {
            binding.status == BindingStatus::SubmitPending
                && binding.request.client_order_id == pending_client_order_id
        }));
        assert_eq!(next.reason, Some(DecisionReason::SubmitPlanned));
    }

    #[test]
    fn pre_receipt_lifecycle_owns_operation_without_replacement_submit() {
        let config = config();
        let rules = rules();

        for status in [
            BindingStatus::SubmitAuthorized,
            BindingStatus::CancelAfterSubmit,
        ] {
            let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
            let existing = previous
                .bindings
                .iter_mut()
                .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
                .expect("catch-up binding should exist");
            existing.status = status;
            let client_order_id = existing.request.client_order_id.clone();

            let next = plan(ExecutorInput::new(
                SubmitIntentInput {
                    instrument: instrument(),
                    config: &config,
                    instrument_spec: &rules,
                    position_qty_per_unit: 1.0,
                    min_rebalance_units: config.min_rebalance_units,
                    current_exposure: Exposure(0.0),
                    risk_approved_target: Exposure(2.0),
                    execution_target: Exposure(2.0),
                    execution_gate_decision: Default::default(),
                    execution_quote: Some(ExecutionQuote {
                        best_bid: 100.0,
                        best_ask: 100.2,
                    }),
                    policy_context: PolicyContext::Normal,
                    price_execution_gate: PriceExecutionGate::Open,
                    submit_purpose: SubmitPurpose::AutoReconcile,
                    observed_at: Utc::now(),
                },
                Some(&previous),
            ));

            assert!(next.effects.iter().all(|effect| !matches!(
                effect,
                TrackEffect::SubmitOrder { .. } | TrackEffect::CancelOrder { .. }
            )));
            assert!(next.state.bindings.iter().any(|binding| {
                binding.request.client_order_id == client_order_id && binding.status == status
            }));
            assert_eq!(
                next.reason,
                Some(match status {
                    BindingStatus::SubmitAuthorized => DecisionReason::SubmitPlanned,
                    BindingStatus::CancelAfterSubmit => DecisionReason::WaitingCancel,
                    _ => unreachable!("test covers pre-receipt lifecycle states only"),
                })
            );
        }
    }

    #[test]
    fn working_reduce_only_catch_up_blocks_replacement_until_terminal() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(-6.0), Exposure(-1.0))).state;
        let existing = previous
            .bindings
            .iter_mut()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be planned");
        existing.status = BindingStatus::Working;
        existing.order_id = Some("existing-reduce-order".to_string());
        existing.request.price = 100.3;
        let existing_client_order_id = existing.request.client_order_id.clone();

        let next = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(-6.0),
                risk_approved_target: Exposure(-1.0),
                execution_target: Exposure(-1.0),
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
            Some(&previous),
        ));

        assert!(
            !next.effects.iter().any(|effect| matches!(
                effect,
                TrackEffect::SubmitOrder { .. } | TrackEffect::CancelOrder { .. }
            )),
            "working reduce-only catch-up must not overlap itself with cancel-and-replace"
        );
        assert!(next.state.bindings.iter().any(|binding| {
            binding.status == BindingStatus::Working
                && binding.request.client_order_id == existing_client_order_id
        }));
    }

    #[test]
    fn curve_maker_policy_keeps_one_passive_binding_per_operation() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(1.0), Exposure(1.0)));

        let maker_bindings = plan
            .state
            .bindings
            .iter()
            .filter(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
            .collect::<Vec<_>>();
        let operations = maker_bindings
            .iter()
            .flat_map(|binding| {
                binding
                    .allocations
                    .iter()
                    .map(|allocation| allocation.operation.clone())
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(maker_bindings.len(), 1);
        assert_eq!(operations.len(), 1);
        assert!(
            maker_bindings
                .iter()
                .all(|binding| binding.allocations.len() == 1)
        );
        assert!(
            maker_bindings.iter().all(|binding| {
                binding.request.side == Side::Sell && binding.request.reduce_only
            })
        );
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
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(2.0),
                execution_target: Exposure(2.0),
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
            Some(&previous),
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. } if order_id == "existing-order"
        )));
        let replacement = plan
            .effects
            .iter()
            .find_map(|effect| match effect {
                TrackEffect::SubmitOrder { request, .. }
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
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(2.0),
                execution_target: Exposure(2.0),
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
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(2.0),
                execution_target: Exposure(2.0),
                execution_gate_decision: Default::default(),
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
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. })),
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
        assert_eq!(next.reason, Some(DecisionReason::WaitingCancel));
    }

    #[test]
    fn gate_cancels_existing_increase_inventory_working_order() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        previous.bindings[0].status = BindingStatus::Working;
        previous.bindings[0].order_id = Some("increase-order".to_string());

        let blocked = plan(input_with_gate(
            &config,
            &rules,
            Exposure(0.0),
            Exposure(0.0),
            PriceExecutionGate::ManualRiskReductionOnly {
                reason: crate::price_gate::PriceExecutionBlockReason::MarkBookDivergence,
            },
            &previous,
        ));

        assert!(blocked.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. } if order_id == "increase-order"
        )));
        assert!(
            !blocked
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. }))
        );
        assert_eq!(
            blocked.state.bindings[0].status,
            BindingStatus::CancelPending
        );
        assert_eq!(blocked.reason, Some(DecisionReason::PriceGateBlocked));

        let waiting = plan(input_with_gate(
            &config,
            &rules,
            Exposure(0.0),
            Exposure(0.0),
            PriceExecutionGate::ManualRiskReductionOnly {
                reason: crate::price_gate::PriceExecutionBlockReason::MarkBookDivergence,
            },
            &blocked.state,
        ));

        assert!(waiting.effects.is_empty());
        assert_eq!(waiting.reason, Some(DecisionReason::WaitingCancel));
    }

    #[test]
    fn execution_gate_cancels_existing_increase_and_blocks_replacement_submit() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        previous.bindings[0].status = BindingStatus::Working;
        previous.bindings[0].order_id = Some("capacity-blocked-increase".to_string());

        let plan = plan(input_with_execution_gate(
            &config,
            &rules,
            Exposure(0.0),
            Exposure(2.0),
            ExecutionGateDecision::NoSubmit {
                reason: poise_core::events::ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 200.0,
                    available_notional: 0.0,
                },
            },
            &previous,
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. }
                if order_id == "capacity-blocked-increase"
        )));
        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. }))
        );
        assert_eq!(plan.state.bindings[0].status, BindingStatus::CancelPending);
    }

    #[test]
    fn execution_gate_defers_cancel_until_authorized_submit_gets_order_identity() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(2.0))).state;
        previous.bindings[0].status = BindingStatus::SubmitAuthorized;

        let plan = plan(input_with_execution_gate(
            &config,
            &rules,
            Exposure(0.0),
            Exposure(2.0),
            ExecutionGateDecision::NoSubmit {
                reason: poise_core::events::ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 200.0,
                    available_notional: 0.0,
                },
            },
            &previous,
        ));

        assert_eq!(plan.state.bindings.len(), 1);
        assert_eq!(
            plan.state.bindings[0].status,
            BindingStatus::CancelAfterSubmit
        );
        assert!(plan.state.bindings[0].is_active());
        assert!(
            plan.effects.iter().all(|effect| !matches!(
                effect,
                TrackEffect::SubmitOrder { .. } | TrackEffect::CancelOrder { .. }
            )),
            "authorized submit has no exchange order id yet and must not be replaced"
        );
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
                .any(|effect| matches!(effect, TrackEffect::CancelOrder { .. }))
        );
        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. }))
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
        let boundary_id = BoundaryId {
            profile_revision: ProfileRevision(previous.ledger_state.profile_revision.0.clone()),
            lower_exposure_bp: 0,
            upper_exposure_bp: 10_000,
        };
        previous.ledger_state.progress.insert(
            boundary_id,
            BoundaryProgress {
                cumulative_up: 1.2,
                cumulative_down: 0.0,
            },
        );

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
        assert!(plan.effects.is_empty());
        assert_eq!(plan.reason, Some(DecisionReason::RecoveryRequired));
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
                .any(|effect| matches!(effect, TrackEffect::CancelOrder { .. }))
        );
        assert_eq!(plan.state.bindings[0].status, BindingStatus::Working);
    }

    #[test]
    fn catch_up_cancels_due_reduce_only_curve_maker_before_submit() {
        let config = config();
        let rules = rules();
        let initial = plan(input(&config, &rules, Exposure(1.0), Exposure(1.0))).state;
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
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(1.0),
                risk_approved_target: Exposure(0.0),
                execution_target: Exposure(0.0),
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
            Some(&previous),
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. } if order_id == "maker-order"
        )));
        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. })),
            "reduce-only catch-up must wait for the maker cancel to finish before submitting"
        );
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CurveMaker
                && binding.status == BindingStatus::CancelPending
        }));
        assert!(!plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CatchUp
                && binding.status == BindingStatus::SubmitPending
        }));
    }

    #[test]
    fn catch_up_cancels_far_reduce_only_curve_maker_before_submit() {
        let config = config();
        let rules = rules();
        let initial = plan(input(&config, &rules, Exposure(1.0), Exposure(1.0))).state;
        let mut previous = initial.clone();
        let maker = previous
            .bindings
            .iter_mut()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
            .expect("maker binding should exist");
        maker.status = BindingStatus::Working;
        maker.order_id = Some("maker-order".to_string());
        maker.request.price = 100.2;
        maker.policy_state = BindingPolicyState::CurveMaker {
            due_grace_started_at: None,
        };

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(1.0),
                risk_approved_target: Exposure(0.0),
                execution_target: Exposure(0.0),
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
            Some(&previous),
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. } if order_id == "maker-order"
        )));
        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. })),
            "far reduce-only maker must be canceled before catch-up submits a replacement"
        );
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CurveMaker
                && binding.status == BindingStatus::CancelPending
        }));
        assert!(!plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CatchUp
                && binding.status == BindingStatus::SubmitPending
        }));
    }

    #[test]
    fn catch_up_keeps_nearby_working_curve_maker_that_can_cover_gap() {
        let config = config();
        let rules = rules();
        let initial = plan(input(&config, &rules, Exposure(1.0), Exposure(1.0))).state;
        let mut previous = initial.clone();
        let maker = previous
            .bindings
            .iter_mut()
            .find(|binding| {
                binding.proposal_key.policy == PolicyKind::CurveMaker
                    && binding.request.side == Side::Sell
            })
            .expect("sell maker binding should exist");
        maker.status = BindingStatus::Working;
        maker.order_id = Some("near-maker-order".to_string());
        maker.request.price = 99.9;
        maker.policy_state = BindingPolicyState::CurveMaker {
            due_grace_started_at: None,
        };

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(1.0),
                risk_approved_target: Exposure(0.0),
                execution_target: Exposure(0.0),
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
            Some(&previous),
        ));

        assert!(
            !plan.effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::CancelOrder { order_id, .. } if order_id == "near-maker-order"))
        );
        assert!(
            !plan
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { request, .. } if request.client_order_id.starts_with("bc")))
        );
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.order_id.as_deref() == Some("near-maker-order")
                && binding.status == BindingStatus::Working
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
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(0.0),
                execution_target: Exposure(0.0),
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
    fn planning_removes_terminal_bindings_on_recovery_anomaly_return() {
        let config = config();
        let rules = rules();
        let mut previous = plan(input(&config, &rules, Exposure(0.0), Exposure(1.0))).state;
        previous.bindings[0].status = BindingStatus::Terminal;
        previous.recovery_anomaly =
            Some(crate::executor::RecoveryAnomaly::ExpectedExposureMismatch);

        let next = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: config.min_rebalance_units,
                current_exposure: Exposure(0.0),
                risk_approved_target: Exposure(0.0),
                execution_target: Exposure(0.0),
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
    fn curve_maker_policy_emits_buy_side_reduce_only_binding_for_short_retrace() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(-1.0), Exposure(-1.0)));

        let buy_maker = plan
            .state
            .bindings
            .iter()
            .find(|binding| {
                binding.proposal_key.policy == PolicyKind::CurveMaker
                    && binding.request.side == Side::Buy
                    && binding.request.reduce_only
            })
            .expect("buy-side short retrace maker binding should be reduce-only");
        assert!(!buy_maker.increases_inventory());
        assert_eq!(buy_maker.allocations.len(), 1);
    }

    #[test]
    fn planning_uses_process_local_binding_ids() {
        let binding_id = next_binding_id(PolicyKind::CatchUp);
        let binding_suffix = binding_id
            .strip_prefix("binding-catch-up-")
            .expect("binding id should keep the catch-up prefix");
        uuid::Uuid::parse_str(binding_suffix).expect("binding id suffix should be a UUID");
    }

    #[test]
    fn planning_enters_expected_exposure_mismatch_anomaly_when_ledger_drift_is_unexplained() {
        let config = config();
        let rules = rules();
        let mut state = ExecutorState::empty(Utc::now()).ensure_revision(&config, Exposure(0.0));
        let boundary_id = BoundaryId {
            profile_revision: ProfileRevision(state.ledger_state.profile_revision.0.clone()),
            lower_exposure_bp: 0,
            upper_exposure_bp: 10_000,
        };
        state.ledger_state.progress.insert(
            boundary_id,
            BoundaryProgress {
                cumulative_up: 1.0,
                cumulative_down: 0.0,
            },
        );

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
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
            Some(&state),
        ));

        assert_eq!(
            plan.state.recovery_anomaly,
            Some(crate::executor::RecoveryAnomaly::ExpectedExposureMismatch)
        );
        assert!(plan.effects.is_empty());
        assert_eq!(plan.reason, Some(DecisionReason::RecoveryRequired));
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
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(0.6),
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
            Some(&previous),
        ));

        assert_eq!(plan.state.recovery_anomaly, None);
    }

    #[test]
    fn planning_reports_boundary_progress_out_of_range_before_using_ledger_view() {
        let config = config();
        let rules = rules();
        let mut state = ExecutorState::empty(Utc::now()).ensure_revision(&config, Exposure(0.0));
        let boundary_id = BoundaryId {
            profile_revision: ProfileRevision(state.ledger_state.profile_revision.0.clone()),
            lower_exposure_bp: 0,
            upper_exposure_bp: 10_000,
        };
        state.ledger_state.progress.insert(
            boundary_id,
            BoundaryProgress {
                cumulative_up: 1.2,
                cumulative_down: 0.0,
            },
        );

        let plan = plan(ExecutorInput::new(
            SubmitIntentInput {
                instrument: instrument(),
                config: &config,
                instrument_spec: &rules,
                position_qty_per_unit: 1.0,
                min_rebalance_units: 1.0,
                current_exposure: Exposure(1.2),
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
            Some(&state),
        ));

        assert_eq!(
            plan.state.recovery_anomaly,
            Some(crate::executor::RecoveryAnomaly::BoundaryProgressOutOfRange)
        );
        assert!(plan.effects.is_empty());
        assert_eq!(plan.reason, Some(DecisionReason::RecoveryRequired));
    }
}
