use chrono::{DateTime, Utc};
use poise_core::strategy::TrackConfig;
use poise_core::types::{ExchangeRules, Exposure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

use crate::execution_plan::ExecutionAction;
use crate::ports::{ExecutionQuote, OrderRequest};
use crate::price_gate::{
    PriceExecutionGate, SubmitPurpose, WorkingOrderGateAction, allows_submit,
    working_order_gate_action,
};
use crate::runtime::ExecutorState;
use crate::track::Instrument;

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

    let mut effects =
        apply_price_gate_to_existing_bindings(&mut state, submit_intent.price_execution_gate);

    if !allows_submit(
        submit_intent.price_execution_gate,
        submit_intent.submit_purpose,
    ) {
        return finish_plan(state, Vec::new(), effects);
    }

    if submit_intent.policy_context == PolicyContext::Normal {
        // This only starts or clears the due grace timer. Reconciliation owns
        // the later choice to keep, cancel, or replace an existing maker.
        refresh_curve_maker_due_grace_start(&mut state, &view, submit_intent.observed_at);
    }
    let policy_input = PolicyPlanningInput {
        view: &view,
        boundaries: &boundaries,
        instrument: submit_intent.instrument,
        config: submit_intent.config,
        exchange_rules: submit_intent.exchange_rules,
        base_qty_per_unit: submit_intent.base_qty_per_unit,
        min_rebalance_units: submit_intent.min_rebalance_units,
        current_exposure: &submit_intent.current_exposure,
        desired_exposure: &submit_intent.desired_exposure,
        execution_quote: submit_intent.execution_quote,
        submit_purpose: submit_intent.submit_purpose,
        exposure_epsilon,
        curve_maker_levels_per_side: CURVE_MAKER_LEVELS_PER_SIDE,
    };

    let desired_bindings = plan_policy_bindings(submit_intent.policy_context, &policy_input);
    effects.extend(reconcile_bindings(
        &mut state,
        &desired_bindings,
        submit_intent.exchange_rules,
        submit_intent.observed_at,
        CURVE_MAKER_GRACE_MS,
    ));

    finish_plan(
        state,
        desired_bindings
            .iter()
            .map(|binding| binding.proposal.clone())
            .collect(),
        effects,
    )
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
    finish_plan(state, Vec::new(), vec![ExecutionAction::NoOp])
}

fn finish_plan(
    mut state: ExecutorState,
    desired_bindings: Vec<BindingProposal>,
    mut effects: Vec<ExecutionAction>,
) -> ExecutorPlan {
    if effects.is_empty() {
        effects.push(ExecutionAction::NoOp);
    }
    // Terminal bindings are an ExecutorPlan output invariant, including early returns.
    state
        .bindings
        .retain(|binding| binding.status != BindingStatus::Terminal);

    ExecutorPlan {
        state,
        desired_bindings,
        effects,
    }
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

        due_grace_started_at.get_or_insert(observed_at);
    }
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

fn reconcile_bindings(
    state: &mut ExecutorState,
    desired_bindings: &[DesiredBinding],
    exchange_rules: &ExchangeRules,
    observed_at: DateTime<Utc>,
    curve_maker_grace_ms: i64,
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

fn exposure_epsilon(input: &SubmitIntentInput<'_>) -> f64 {
    let quantity_step_as_exposure = if input.base_qty_per_unit <= f64::EPSILON {
        0.0
    } else {
        input.exchange_rules.quantity_step / input.base_qty_per_unit
    };
    (input.min_rebalance_units * 0.01).max(quantity_step_as_exposure)
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
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Side};
    use std::collections::BTreeSet;
    use std::sync::LazyLock;

    use super::*;
    use crate::executor::boundary::{BoundaryId, ProfileRevision};
    use crate::executor::ledger::BoundaryProgress;
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
        assert_eq!(catch_up_bindings[0].allocations.len(), 3);
        assert!((catch_up_bindings[0].request.quantity - 3.0).abs() < 1e-9);
        assert_eq!(
            plan.effects
                .iter()
                .filter(|effect| matches!(effect, ExecutionAction::SubmitOrder { request, .. } if request.client_order_id.starts_with("bc-")))
                .count(),
            1
        );
    }

    #[test]
    fn curve_maker_policy_keeps_one_passive_binding_per_operation() {
        let config = config();
        let rules = rules();

        let plan = plan(input(&config, &rules, Exposure(0.0), Exposure(0.0)));

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
        assert_eq!(maker_bindings.len(), 6);
        assert_eq!(operations.len(), 6);
        assert!(
            maker_bindings
                .iter()
                .all(|binding| binding.allocations.len() == 1)
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
