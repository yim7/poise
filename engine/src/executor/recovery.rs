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
        let matches = state
            .bindings
            .iter()
            .enumerate()
            .filter_map(|(index, binding)| {
                (binding.request.client_order_id == live_order.client_order_id
                    || binding.order_id.as_deref() == Some(live_order.order_id.as_str()))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let [index] = matches.as_slice() else {
            return recovery_anomaly(state, RecoveryAnomaly::UnknownLiveOrder);
        };
        if claimed[*index] {
            return recovery_anomaly(state, RecoveryAnomaly::DuplicateLiveOrders);
        }
        claimed[*index] = true;
        state.bindings[*index].order_id = Some(live_order.order_id.clone());
        state.bindings[*index].request.price = live_order.price;
        state.bindings[*index].request.quantity = live_order.quantity;
        state.bindings[*index].status = BindingStatus::Working;
    }

    state.recovery_anomaly = None;
    RecoveryResolution::Rebuilt { state }
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

    use super::*;

    #[test]
    fn recovery_does_not_fabricate_boundary_progress_from_live_order_alone() {
        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(0.0),
            desired_exposure: None,
            min_rebalance_units: 1.0,
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
