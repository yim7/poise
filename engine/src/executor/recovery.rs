use chrono::{DateTime, Utc};
use poise_core::types::{ExchangeRules, Exposure};
use serde::{Deserialize, Serialize};

use crate::observation::OrderObservation;
use crate::ports::{OrderReceipt, OrderRequest};
use crate::runtime::ExecutorState;

use super::binding::BindingStatus;
use super::planning::{PendingSubmitHint, SubmitIntentInput, current_submit_hint};
use super::recording;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAnomaly {
    UnknownLiveOrder,
    AmbiguousLiveOrder,
    DuplicateLiveOrders,
    ExpectedExposureMismatch,
}

pub struct RecoveryInput<'a> {
    #[allow(dead_code)]
    pub current_exposure: &'a Exposure,
    #[allow(dead_code)]
    pub desired_exposure: Option<&'a Exposure>,
    #[allow(dead_code)]
    pub min_rebalance_units: f64,
    pub exchange_rules: &'a ExchangeRules,
    pub previous_state: Option<&'a ExecutorState>,
    pub live_orders: &'a [OrderObservation],
    #[allow(dead_code)]
    pub pending_submit_hints: &'a [PendingSubmitHint],
    pub observed_at: DateTime<Utc>,
}

pub enum RecoveryResolution {
    Rebuilt {
        state: ExecutorState,
    },
    Anomaly {
        state: ExecutorState,
        #[allow(dead_code)]
        anomaly: RecoveryAnomaly,
    },
}

pub struct SubmitRecoveryInput<'a> {
    #[allow(dead_code)]
    pub exchange_rules: &'a ExchangeRules,
    pub previous_state: &'a ExecutorState,
    pub request: &'a OrderRequest,
    pub desired_exposure: &'a Exposure,
    #[allow(dead_code)]
    pub current_exposure: &'a Exposure,
    pub live_order: Option<&'a OrderObservation>,
    pub current_plan: Option<SubmitIntentInput<'a>>,
}

pub enum SubmitRecoveryResolution {
    Proceed {
        request: OrderRequest,
        desired_exposure: Exposure,
    },
    Recovered {
        state: ExecutorState,
    },
    Superseded {
        state: ExecutorState,
    },
    AwaitExchangeState,
}

impl SubmitRecoveryResolution {
    pub fn recovered_state(&self) -> Option<&ExecutorState> {
        match self {
            Self::Recovered { state } | Self::Superseded { state } => Some(state),
            _ => None,
        }
    }

    pub fn state(&self) -> Option<&ExecutorState> {
        self.recovered_state()
    }
}

pub struct SubmitRecoveryPlan {
    pub resolution: SubmitRecoveryResolution,
}

pub fn recover_submit_effect(input: SubmitRecoveryInput<'_>) -> SubmitRecoveryPlan {
    if let Some(live_order) = input.live_order {
        let receipt = OrderReceipt {
            order_id: live_order.order_id.clone(),
            client_order_id: live_order.client_order_id.clone(),
            status: live_order.status,
        };
        let resolution = recording::record_submit_receipt(
            input.previous_state,
            input.request,
            input.desired_exposure.clone(),
            &receipt,
        );
        if let recording::SubmitReceiptResolution::Recorded { state } = resolution {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Recovered { state },
            };
        }
    }

    if let Some(current) = input.current_plan.and_then(current_submit_hint) {
        return SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Proceed {
                request: current.request.clone(),
                desired_exposure: current.desired_exposure.clone(),
            },
        };
    }

    SubmitRecoveryPlan {
        resolution: SubmitRecoveryResolution::AwaitExchangeState,
    }
}

pub fn recover_working_orders(input: RecoveryInput<'_>) -> RecoveryResolution {
    let mut state = input
        .previous_state
        .cloned()
        .unwrap_or_else(|| ExecutorState::empty(input.observed_at));

    if input.live_orders.is_empty() {
        state
            .bindings
            .retain(|binding| binding.status == BindingStatus::SubmitPending);
        state.recovery_anomaly = None;
        return RecoveryResolution::Rebuilt { state };
    }

    let mut claimed = vec![false; state.bindings.len()];
    for live_order in input.live_orders {
        let matches = binding_candidates_for_live_order(&state, live_order, input.exchange_rules);
        let index = match matches.as_slice() {
            [] => return recovery_anomaly(state, RecoveryAnomaly::UnknownLiveOrder),
            [index] => *index,
            _ => return recovery_anomaly(state, RecoveryAnomaly::AmbiguousLiveOrder),
        };
        if claimed[index] {
            return recovery_anomaly(state, RecoveryAnomaly::DuplicateLiveOrders);
        }
        claimed[index] = true;
        state.bindings[index].order_id = Some(live_order.order_id.clone());
        state.bindings[index].request.price = live_order.price;
        state.bindings[index].request.quantity = live_order.quantity;
        state.bindings[index].status = BindingStatus::Working;
    }

    state.recovery_anomaly = None;
    RecoveryResolution::Rebuilt { state }
}

fn binding_candidates_for_live_order(
    state: &ExecutorState,
    live_order: &OrderObservation,
    rules: &ExchangeRules,
) -> Vec<usize> {
    let id_matches = state
        .bindings
        .iter()
        .enumerate()
        .filter_map(|(index, binding)| {
            (binding.request.client_order_id == live_order.client_order_id
                || binding.order_id.as_deref() == Some(live_order.order_id.as_str()))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    if !id_matches.is_empty() {
        return id_matches;
    }

    state
        .bindings
        .iter()
        .enumerate()
        .filter(|(_, binding)| {
            matches!(
                binding.status,
                BindingStatus::SubmitPending | BindingStatus::Working
            )
        })
        .filter(|(_, binding)| binding.request.side == live_order.side)
        .filter(|(_, binding)| {
            values_match(binding.request.price, live_order.price, rules.price_tick)
        })
        .filter(|(_, binding)| {
            values_match(
                binding.request.quantity,
                live_order.quantity,
                rules.quantity_step,
            )
        })
        .map(|(index, _)| index)
        .collect()
}

fn values_match(expected: f64, observed: f64, tolerance: f64) -> bool {
    let tolerance = tolerance.max(f64::EPSILON);
    (expected - observed).abs() <= tolerance + f64::EPSILON
}

pub(crate) fn submit_requests_match(
    left: &OrderRequest,
    right: &OrderRequest,
    _rules: &ExchangeRules,
) -> bool {
    left.instrument == right.instrument
        && left.side == right.side
        && (left.price - right.price).abs() < f64::EPSILON
        && (left.quantity - right.quantity).abs() < f64::EPSILON
        && left.reduce_only == right.reduce_only
}

fn recovery_anomaly(mut state: ExecutorState, anomaly: RecoveryAnomaly) -> RecoveryResolution {
    state.recovery_anomaly = Some(anomaly.clone());
    RecoveryResolution::Anomaly { state, anomaly }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::types::{ExchangeRules, Side};

    use super::*;
    use crate::executor::binding::{
        BindingOperationAllocation, BindingPolicyState, BindingProposal, BindingStatus,
        LiveOrderBinding,
    };
    use crate::executor::boundary::{
        BoundaryDirection, BoundaryId, BoundaryOperation, ProfileRevision,
    };
    use crate::executor::policy::PolicyKind;
    use crate::ports::OrderStatus;
    use crate::price_gate::SubmitPurpose;
    use crate::track::{Instrument, Venue};

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

    fn operation() -> BoundaryOperation {
        BoundaryOperation {
            boundary_id: BoundaryId {
                profile_revision: ProfileRevision("rev-1".to_string()),
                lower_exposure_bp: 0,
                upper_exposure_bp: 10_000,
            },
            direction: BoundaryDirection::Up,
        }
    }

    fn binding(client_order_id: &str, side: Side, price: f64, quantity: f64) -> LiveOrderBinding {
        let operation = operation();
        let proposal = BindingProposal {
            policy: PolicyKind::CurveMaker,
            operations: vec![operation.clone()],
        };
        LiveOrderBinding {
            binding_id: client_order_id.to_string(),
            proposal_key: proposal.proposal_key(),
            allocations: vec![BindingOperationAllocation {
                operation,
                exposure_qty: quantity,
            }],
            request: OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side,
                price,
                quantity,
                client_order_id: client_order_id.to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(1.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            order_id: None,
            status: BindingStatus::SubmitPending,
            policy_state: BindingPolicyState::CurveMaker {
                due_grace_started_at: None,
            },
        }
    }

    fn live_order(
        client_order_id: &str,
        side: Side,
        price: f64,
        quantity: f64,
    ) -> OrderObservation {
        OrderObservation {
            order_id: "live-order-1".to_string(),
            client_order_id: client_order_id.to_string(),
            side,
            price,
            quantity,
            realized_pnl: 0.0,
            status: OrderStatus::New,
        }
    }

    fn recover_with(
        previous_state: &ExecutorState,
        live_orders: &[OrderObservation],
    ) -> RecoveryResolution {
        let rules = rules();
        recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(0.0),
            desired_exposure: None,
            min_rebalance_units: 1.0,
            exchange_rules: &rules,
            previous_state: Some(previous_state),
            live_orders,
            pending_submit_hints: &[],
            observed_at: Utc::now(),
        })
    }

    #[test]
    fn recovery_matches_live_order_to_single_expected_binding_candidate() {
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state
            .bindings
            .push(binding("expected-client", Side::Buy, 100.0, 1.0));
        let live_orders = vec![live_order("exchange-client", Side::Buy, 100.04, 1.004)];

        let recovery = recover_with(&previous_state, &live_orders);

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected recovery to rebuild from structural match");
        };
        assert_eq!(state.bindings[0].order_id.as_deref(), Some("live-order-1"));
        assert_eq!(state.bindings[0].status, BindingStatus::Working);
        assert!(state.ledger_state.progress.is_empty());
    }

    #[test]
    fn recovery_marks_unknown_live_order_when_no_binding_candidate_matches() {
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state
            .bindings
            .push(binding("expected-client", Side::Buy, 100.0, 1.0));
        let live_orders = vec![live_order("exchange-client", Side::Sell, 100.0, 1.0)];

        let recovery = recover_with(&previous_state, &live_orders);

        let RecoveryResolution::Anomaly { state, anomaly } = recovery else {
            panic!("expected unknown live order anomaly");
        };
        assert_eq!(anomaly, RecoveryAnomaly::UnknownLiveOrder);
        assert_eq!(
            state.recovery_anomaly,
            Some(RecoveryAnomaly::UnknownLiveOrder)
        );
        assert!(state.ledger_state.progress.is_empty());
    }

    #[test]
    fn recovery_marks_ambiguous_live_order_when_multiple_binding_candidates_match() {
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state
            .bindings
            .push(binding("expected-client-1", Side::Buy, 100.0, 1.0));
        previous_state
            .bindings
            .push(binding("expected-client-2", Side::Buy, 100.0, 1.0));
        let live_orders = vec![live_order("exchange-client", Side::Buy, 100.0, 1.0)];

        let recovery = recover_with(&previous_state, &live_orders);

        let RecoveryResolution::Anomaly { state, anomaly } = recovery else {
            panic!("expected ambiguous live order anomaly");
        };
        assert_eq!(anomaly, RecoveryAnomaly::AmbiguousLiveOrder);
        assert_eq!(
            state.recovery_anomaly,
            Some(RecoveryAnomaly::AmbiguousLiveOrder)
        );
        assert!(state.ledger_state.progress.is_empty());
    }

    #[test]
    fn recovery_does_not_fabricate_boundary_progress_from_live_order_alone() {
        let rules = rules();
        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(0.0),
            desired_exposure: None,
            min_rebalance_units: 1.0,
            exchange_rules: &rules,
            previous_state: Some(&ExecutorState::empty(Utc::now())),
            live_orders: &[],
            pending_submit_hints: &[],
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected rebuilt state");
        };
        assert!(state.ledger_state.progress.is_empty());
    }
}
