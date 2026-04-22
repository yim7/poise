pub(crate) mod binding;
pub(crate) mod boundary;
pub(crate) mod ledger;
mod planning;
pub(crate) mod policy;
mod recording;
mod recovery;

pub(crate) use planning::{ExecutorInput, SubmitIntentInput, plan, refresh_state};
pub use planning::{OrderRole, PendingSubmitHint};
pub use recording::OrderUpdateAbsorbResult;
pub(crate) use recording::{
    SubmitReceiptResolution, apply_order_observation_with_result, clear_all_working_orders,
    clear_working_order_by_order_id, record_submit_failure, record_submit_receipt,
    record_submit_request,
};
pub use recovery::{RecoveryAnomaly, SubmitRecoveryPlan, SubmitRecoveryResolution};
pub(crate) use recovery::{
    RecoveryInput, RecoveryResolution, SubmitRecoveryInput, recover_submit_effect,
    recover_working_orders, submit_requests_match,
};

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};

    use super::*;
    use crate::execution_plan::ExecutionAction;
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
    fn catch_up_policy_preempts_curve_maker_after_due_grace_expires() {
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
            ExecutionAction::CancelOrder { order_id, .. } if order_id == "maker-order"
        )));
        assert!(plan.state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CatchUp
                && binding.allocations.iter().any(|allocation| {
                    allocation.operation.direction == BoundaryDirection::Up
                        && (allocation.exposure_qty - 1.0).abs() < 1e-9
                })
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
    fn binding_diff_replaces_maker_binding_with_catch_up_binding_on_preemption() {
        let config = config();
        let rules = rules();
        let instrument = instrument();
        let maker_operation = operation(&config, 0.0, 1.0, BoundaryDirection::Up);
        let mut state = ExecutorState::empty(observed_at()).ensure_revision(&config, Exposure(0.0));
        state
            .bindings
            .push(maker_binding(&config, maker_operation.clone()));

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
            .find(|binding| binding.proposal_key.policy == PolicyKind::CurveMaker)
            .expect("maker binding should still be tracked while canceling");
        assert_eq!(maker.status, BindingStatus::CancelPending);

        let catch_up = plan
            .state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up replacement should be submitted");
        assert_eq!(catch_up.allocations[0].operation, maker_operation);
    }
}
