pub(crate) mod binding;
pub(crate) mod boundary;
pub(crate) mod ledger;
mod planning;
pub(crate) mod policy;
mod recording;
mod recovery;

pub use binding::{BindingStatus, SubmitRecoveryToken};
pub(crate) use planning::{ExecutorInput, SubmitIntentInput, plan, refresh_state};
pub use planning::{OrderRole, PendingSubmitHint};
pub use policy::{PolicyContext, PolicyKind};
pub use recording::OrderUpdateAbsorbResult;
pub(crate) use recording::{
    SubmitReceiptResolution, apply_order_observation_with_result, clear_all_working_orders,
    record_cancel_order_receipt, record_submit_failure, record_submit_failure_by_recovery_token,
    record_submit_receipt,
};
pub use recovery::{RecoveryAnomaly, SubmitRecoveryPlan, SubmitRecoveryResolution};
pub(crate) use recovery::{
    RecoveryInput, RecoveryResolution, SubmitRecoveryInput, recover_submit_effect,
    recover_working_orders,
};

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};

    use super::*;
    use crate::execution_plan::TrackEffect;
    use crate::executor::binding::{
        BindingOperationAllocation, BindingPolicyState, BindingStatus, LiveOrderBinding,
    };
    use crate::executor::boundary::{
        BoundaryDirection, BoundaryOperation, discretize_boundaries, profile_revision_for_config,
    };
    use crate::executor::policy::PolicyKind;
    use crate::ports::{ExecutionQuote, OrderRequest};
    use crate::price_gate::{PriceExecutionGate, SubmitPurpose};
    use crate::runtime::ExecutorState;
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
        }
    }

    fn rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_step: 0.01,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    fn instrument() -> Instrument {
        Instrument::new(Venue::Binance, "BTCUSDT")
    }

    fn observed_at() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap()
    }

    fn input<'a>(
        config: &'a TrackConfig,
        rules: &'a ExchangeRules,
        instrument: &'a Instrument,
        current_exposure: Exposure,
        desired_exposure: Exposure,
        state: Option<&'a ExecutorState>,
    ) -> ExecutorInput<'a> {
        input_with_context(
            config,
            rules,
            instrument,
            current_exposure,
            desired_exposure,
            PolicyContext::Normal,
            state,
        )
    }

    fn input_with_context<'a>(
        config: &'a TrackConfig,
        rules: &'a ExchangeRules,
        instrument: &'a Instrument,
        current_exposure: Exposure,
        desired_exposure: Exposure,
        policy_context: PolicyContext,
        state: Option<&'a ExecutorState>,
    ) -> ExecutorInput<'a> {
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
                policy_context,
                price_execution_gate: PriceExecutionGate::Open,
                submit_purpose: SubmitPurpose::AutoReconcile,
                observed_at: observed_at(),
            },
            state,
        )
    }

    fn operation(
        config: &TrackConfig,
        lower: f64,
        upper: f64,
        direction: BoundaryDirection,
    ) -> BoundaryOperation {
        let boundary = discretize_boundaries(config, profile_revision_for_config(config))
            .into_iter()
            .find(|boundary| {
                (boundary.lower_exposure.0 - lower).abs() < 1e-9
                    && (boundary.upper_exposure.0 - upper).abs() < 1e-9
            })
            .expect("boundary should exist");
        BoundaryOperation {
            boundary_id: boundary.id,
            direction,
        }
    }

    fn maker_binding(config: &TrackConfig, operation: BoundaryOperation) -> LiveOrderBinding {
        let proposal = binding::BindingProposal {
            policy: PolicyKind::CurveMaker,
            operations: vec![operation.clone()],
        };
        LiveOrderBinding {
            binding_id: "maker-binding".to_string(),
            proposal_key: proposal.proposal_key(),
            allocations: vec![BindingOperationAllocation {
                operation,
                exposure_qty: 1.0,
            }],
            absorbed_exposure_qty: 0.0,
            request: OrderRequest {
                instrument: instrument(),
                side: Side::Buy,
                price: 98.75,
                quantity: config.base_qty_per_unit(),
                client_order_id: "maker-client".to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(0.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            order_id: Some("maker-order".to_string()),
            status: BindingStatus::Working,
            policy_state: BindingPolicyState::CurveMaker {
                due_grace_started_at: Some(observed_at() - Duration::seconds(120)),
            },
        }
    }

    fn catch_up_binding(config: &TrackConfig, operation: BoundaryOperation) -> LiveOrderBinding {
        let proposal = binding::BindingProposal {
            policy: PolicyKind::CatchUp,
            operations: vec![operation.clone()],
        };
        LiveOrderBinding {
            binding_id: "catch-up-binding".to_string(),
            proposal_key: proposal.proposal_key(),
            allocations: vec![BindingOperationAllocation {
                operation,
                exposure_qty: 1.0,
            }],
            absorbed_exposure_qty: 0.0,
            request: OrderRequest {
                instrument: instrument(),
                side: Side::Buy,
                price: 100.1,
                quantity: config.base_qty_per_unit(),
                client_order_id: "catch-up-client".to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(1.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            order_id: Some("catch-up-order".to_string()),
            status: BindingStatus::Working,
            policy_state: BindingPolicyState::Stateless,
        }
    }

    #[test]
    fn curve_maker_policy_emits_future_operations_near_spot() {
        let config = config();
        let rules = rules();
        let instrument = instrument();

        let plan = plan(input(
            &config,
            &rules,
            &instrument,
            Exposure(0.0),
            Exposure(0.0),
            None,
        ));

        let maker_bindings = plan
            .state
            .bindings
            .iter()
            .filter(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
            .collect::<Vec<_>>();
        assert_eq!(maker_bindings.len(), 6);
        assert_eq!(
            maker_bindings
                .iter()
                .filter(|binding| binding.request.side == Side::Buy)
                .count(),
            3
        );
        assert_eq!(
            maker_bindings
                .iter()
                .filter(|binding| binding.request.side == Side::Sell)
                .count(),
            3
        );
        assert!(maker_bindings.iter().all(|binding| matches!(
            binding.policy_state,
            BindingPolicyState::CurveMaker {
                due_grace_started_at: None
            }
        )));
    }

    #[test]
    fn catch_up_policy_cancels_stale_curve_maker_and_takes_over_operation_in_same_round() {
        let config = config();
        let rules = rules();
        let instrument = instrument();
        let mut state = ExecutorState::empty(observed_at()).ensure_revision(&config, Exposure(0.0));
        state.bindings.push(maker_binding(
            &config,
            operation(&config, 0.0, 1.0, BoundaryDirection::Up),
        ));

        let plan = plan(input(
            &config,
            &rules,
            &instrument,
            Exposure(0.0),
            Exposure(1.0),
            Some(&state),
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. } if order_id == "maker-order"
        )));
        let maker = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
            .expect("maker binding should remain tracked while canceling");
        assert_eq!(maker.status, BindingStatus::CancelPending);
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CatchUp
                && binding
                    .allocations
                    .iter()
                    .any(|allocation| allocation.operation.direction == BoundaryDirection::Up)
        }));
    }

    #[test]
    fn curve_maker_policy_state_is_private_to_binding() {
        let config = config();
        let rules = rules();
        let instrument = instrument();

        let plan = plan(input(
            &config,
            &rules,
            &instrument,
            Exposure(0.0),
            Exposure(0.0),
            None,
        ));

        let binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
            .expect("maker binding should exist");
        let json = serde_json::to_value(binding).unwrap();
        assert!(json.get("due_grace_started_at").is_none());
        assert_eq!(
            json.get("policy_state")
                .and_then(|state| state.get("curve_maker")),
            Some(&serde_json::json!({ "due_grace_started_at": null }))
        );
    }

    #[test]
    fn cancel_pending_maker_binding_holds_boundary_operation_until_cancel_resolves() {
        let config = config();
        let rules = rules();
        let instrument = instrument();
        let maker_operation = operation(&config, 0.0, 1.0, BoundaryDirection::Up);
        let mut state = ExecutorState::empty(observed_at()).ensure_revision(&config, Exposure(0.0));
        let mut maker = maker_binding(&config, maker_operation.clone());
        maker.status = BindingStatus::CancelPending;
        state.bindings.push(maker);

        let plan = plan(input(
            &config,
            &rules,
            &instrument,
            Exposure(0.0),
            Exposure(1.0),
            Some(&state),
        ));

        let maker = plan
            .state
            .bindings
            .iter()
            .find(|binding| {
                binding.proposal_key.policy == PolicyKind::CurveMaker
                    && binding.allocations[0].operation == maker_operation
            })
            .expect("canceling maker binding should stay tracked");
        assert_eq!(maker.status, BindingStatus::CancelPending);
        assert!(!plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CatchUp
                && binding.allocations[0].operation == maker_operation
        }));
    }

    #[test]
    fn manual_override_policy_runs_exclusively_without_curve_maker_or_catch_up() {
        let config = config();
        let rules = rules();
        let instrument = instrument();

        let plan = plan(input_with_context(
            &config,
            &rules,
            &instrument,
            Exposure(0.0),
            Exposure(2.0),
            PolicyContext::ManualOverride,
            None,
        ));

        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::ManualOverride
                && binding.request.side == Side::Buy
                && (binding.request.quantity - 2.0).abs() < 1e-9
        }));
        assert!(!plan.state.bindings.iter().any(|binding| {
            matches!(
                binding.proposal_key.policy,
                PolicyKind::CurveMaker | PolicyKind::CatchUp
            )
        }));
    }

    #[test]
    fn reduce_only_policy_only_emits_risk_reducing_binding() {
        let config = config();
        let rules = rules();
        let instrument = instrument();

        let plan = plan(input_with_context(
            &config,
            &rules,
            &instrument,
            Exposure(2.0),
            Exposure(0.0),
            PolicyContext::ReduceOnly,
            None,
        ));

        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::ReduceOnly
                && binding.request.side == Side::Sell
                && binding.request.reduce_only
                && (binding.request.quantity - 2.0).abs() < 1e-9
        }));
        assert!(!plan.state.bindings.iter().any(|binding| {
            matches!(
                binding.proposal_key.policy,
                PolicyKind::CurveMaker | PolicyKind::CatchUp
            )
        }));
    }

    #[test]
    fn stale_catch_up_binding_is_canceled_when_no_longer_desired() {
        let config = config();
        let rules = rules();
        let instrument = instrument();
        let mut state = ExecutorState::empty(observed_at()).ensure_revision(&config, Exposure(1.0));
        state.bindings.push(catch_up_binding(
            &config,
            operation(&config, 0.0, 1.0, BoundaryDirection::Up),
        ));

        let plan = plan(input(
            &config,
            &rules,
            &instrument,
            Exposure(1.0),
            Exposure(1.0),
            Some(&state),
        ));

        assert!(plan.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. } if order_id == "catch-up-order"
        )));
        let binding = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.request.client_order_id == "catch-up-client")
            .expect("existing catch-up binding should remain tracked while canceling");
        assert_eq!(binding.status, BindingStatus::CancelPending);
    }
}
