use chrono::{DateTime, Utc};
use poise_core::strategy::TrackConfig;
use poise_core::types::{ExchangeRules, Exposure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::observation::{CompleteOpenOrderSnapshot, OrderObservation};
use crate::ports::{OrderReceipt, OrderRequest};
use crate::runtime::ExecutorState;

use super::binding::SubmitRecoveryToken;
use super::binding::{BindingStatus, LiveOrderBinding};
use super::boundary::profile_revision_for_config;
use super::ledger::BoundaryLedgerState;
use super::recording;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAnomaly {
    UnknownLiveOrder,
    AmbiguousLiveOrder,
    DuplicateLiveOrders,
    ExpectedExposureMismatch,
    BoundaryProgressOutOfRange,
}

pub struct RecoveryInput<'a> {
    pub config: &'a TrackConfig,
    pub current_exposure: &'a Exposure,
    #[allow(dead_code)]
    pub desired_exposure: Option<&'a Exposure>,
    pub exchange_rules: &'a ExchangeRules,
    pub previous_state: Option<&'a ExecutorState>,
    pub open_orders: &'a CompleteOpenOrderSnapshot,
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
    pub recovery_token: &'a SubmitRecoveryToken,
    #[allow(dead_code)]
    pub current_exposure: &'a Exposure,
    pub live_order: Option<&'a OrderObservation>,
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
    let binding_target =
        binding_target_for_recovery_token(input.previous_state, input.recovery_token);

    if let Some(live_order) = input.live_order {
        if let SubmitBindingRecovery::Recoverable(target)
        | SubmitBindingRecovery::Dispatchable(target) = &binding_target
        {
            let receipt = OrderReceipt {
                order_id: live_order.order_id.clone(),
                client_order_id: live_order.client_order_id.clone(),
                filled_qty: live_order.filled_qty,
                status: live_order.status,
            };
            let resolution = recording::record_submit_receipt(
                input.previous_state,
                &target.request,
                target.desired_exposure.clone(),
                &receipt,
            );
            if let recording::SubmitReceiptResolution::Recorded { state } = resolution {
                return SubmitRecoveryPlan {
                    resolution: SubmitRecoveryResolution::Recovered { state },
                };
            }
        }
    }

    match binding_target {
        SubmitBindingRecovery::Dispatchable(current) => {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Proceed {
                    request: current.request,
                    desired_exposure: current.desired_exposure,
                },
            };
        }
        SubmitBindingRecovery::Superseded => {
            return SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Superseded {
                    state: input.previous_state.clone(),
                },
            };
        }
        SubmitBindingRecovery::Recoverable(_) | SubmitBindingRecovery::Missing => {}
    }

    SubmitRecoveryPlan {
        resolution: SubmitRecoveryResolution::AwaitExchangeState,
    }
}

#[derive(Debug, Clone)]
struct SubmitRecoveryTarget {
    request: OrderRequest,
    desired_exposure: Exposure,
}

enum SubmitBindingRecovery {
    Dispatchable(SubmitRecoveryTarget),
    Recoverable(SubmitRecoveryTarget),
    Superseded,
    Missing,
}

fn binding_target_for_recovery_token(
    previous_state: &ExecutorState,
    recovery_token: &SubmitRecoveryToken,
) -> SubmitBindingRecovery {
    let Some(target) = recovery_token.decode() else {
        return SubmitBindingRecovery::Missing;
    };

    let Some(binding) = previous_state
        .bindings
        .iter()
        .find(|binding| binding.binding_id == target.binding_id)
    else {
        return SubmitBindingRecovery::Missing;
    };

    let target = SubmitRecoveryTarget {
        request: binding.request.clone(),
        desired_exposure: binding.desired_exposure.clone(),
    };

    match binding.status {
        BindingStatus::SubmitPending => SubmitBindingRecovery::Dispatchable(target),
        BindingStatus::Working => SubmitBindingRecovery::Recoverable(target),
        BindingStatus::CancelPending | BindingStatus::Terminal => SubmitBindingRecovery::Superseded,
    }
}

pub fn recover_working_orders(input: RecoveryInput<'_>) -> RecoveryResolution {
    let mut state = input
        .previous_state
        .cloned()
        .unwrap_or_else(|| ExecutorState::empty(input.observed_at));

    if input.open_orders.is_empty() {
        state
            .bindings
            .retain(|binding| binding.status == BindingStatus::SubmitPending);
    } else {
        let mut claimed_binding_indexes = BTreeSet::new();
        for live_order in input.open_orders.orders() {
            let matches =
                binding_candidates_for_live_order(&state, live_order, input.exchange_rules);
            let candidate = match matches.as_slice() {
                [] => return recovery_anomaly(state, RecoveryAnomaly::UnknownLiveOrder),
                [candidate] => candidate.clone(),
                _ => return recovery_anomaly(state, RecoveryAnomaly::AmbiguousLiveOrder),
            };
            if !claimed_binding_indexes.insert(candidate.index()) {
                return recovery_anomaly(state, RecoveryAnomaly::DuplicateLiveOrders);
            }
            match candidate {
                RecoveryBindingCandidate::Existing { index } => {
                    state.bindings[index].order_id = Some(live_order.order_id.clone());
                    state.bindings[index].request.price = live_order.price;
                    state.bindings[index].request.quantity = live_order.quantity;
                    state.bindings[index].status = BindingStatus::Working;
                    state = recording::apply_order_observation(&state, live_order);
                }
            }
        }
        terminalize_exchange_absent_bindings(&mut state, &claimed_binding_indexes);
    }

    reanchor_executor_ledger(&mut state, input.config, input.current_exposure.clone());
    state.recovery_anomaly = None;
    RecoveryResolution::Rebuilt { state }
}

#[derive(Debug, Clone)]
enum RecoveryBindingCandidate {
    Existing { index: usize },
}

impl RecoveryBindingCandidate {
    fn index(&self) -> usize {
        match self {
            Self::Existing { index } => *index,
        }
    }
}

fn terminalize_exchange_absent_bindings(
    state: &mut ExecutorState,
    claimed_binding_indexes: &BTreeSet<usize>,
) {
    for (index, binding) in state.bindings.iter_mut().enumerate() {
        if claimed_binding_indexes.contains(&index) {
            continue;
        }
        if matches!(
            binding.status,
            BindingStatus::Working | BindingStatus::CancelPending
        ) {
            binding.status = BindingStatus::Terminal;
        }
    }
}

fn reanchor_executor_ledger(
    state: &mut ExecutorState,
    config: &TrackConfig,
    current_exposure: Exposure,
) {
    state.ledger_state = BoundaryLedgerState {
        profile_revision: profile_revision_for_config(config),
        ledger_anchor_exposure: current_exposure,
        progress: Vec::new(),
    };
}

fn binding_candidates_for_live_order(
    state: &ExecutorState,
    live_order: &OrderObservation,
    rules: &ExchangeRules,
) -> Vec<RecoveryBindingCandidate> {
    let existing_candidates = state
        .bindings
        .iter()
        .enumerate()
        .map(|(index, _binding)| RecoveryBindingCandidate::Existing { index })
        .collect::<Vec<_>>();

    let id_matches = id_matches_for_live_order(&existing_candidates, state, live_order);
    if !id_matches.is_empty() {
        return id_matches;
    }

    structural_matches_for_live_order(existing_candidates, state, live_order, rules)
}

impl RecoveryBindingCandidate {
    fn binding<'a>(&'a self, state: &'a ExecutorState) -> &'a LiveOrderBinding {
        match self {
            Self::Existing { index } => &state.bindings[*index],
        }
    }
}

fn id_matches_for_live_order(
    candidates: &[RecoveryBindingCandidate],
    state: &ExecutorState,
    live_order: &OrderObservation,
) -> Vec<RecoveryBindingCandidate> {
    candidates
        .iter()
        .filter(|candidate| {
            let binding = candidate.binding(state);
            binding.request.client_order_id == live_order.client_order_id
                || binding.order_id.as_deref() == Some(live_order.order_id.as_str())
        })
        .cloned()
        .collect()
}

fn structural_matches_for_live_order(
    candidates: Vec<RecoveryBindingCandidate>,
    state: &ExecutorState,
    live_order: &OrderObservation,
    rules: &ExchangeRules,
) -> Vec<RecoveryBindingCandidate> {
    candidates
        .into_iter()
        .filter(|candidate| {
            let binding = candidate.binding(state);
            matches!(
                binding.status,
                BindingStatus::SubmitPending
                    | BindingStatus::Working
                    | BindingStatus::CancelPending
            )
        })
        .filter(|candidate| {
            let binding = candidate.binding(state);
            binding.request.side == live_order.side
        })
        .filter(|candidate| {
            let binding = candidate.binding(state);
            values_match(binding.request.price, live_order.price, rules.price_tick)
        })
        .filter(|candidate| {
            let binding = candidate.binding(state);
            values_match(
                binding.request.quantity,
                live_order.quantity,
                rules.quantity_step,
            )
        })
        .collect()
}

fn values_match(expected: f64, observed: f64, tolerance: f64) -> bool {
    let tolerance = tolerance.max(f64::EPSILON);
    (expected - observed).abs() <= tolerance + f64::EPSILON
}

fn recovery_anomaly(mut state: ExecutorState, anomaly: RecoveryAnomaly) -> RecoveryResolution {
    state.recovery_anomaly = Some(anomaly.clone());
    RecoveryResolution::Anomaly { state, anomaly }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Side};

    use super::*;
    use crate::executor::binding::{
        BindingOperationAllocation, BindingPolicyState, BindingProposal, BindingStatus,
        LiveOrderBinding, SubmitRecoveryToken,
    };
    use crate::executor::boundary::{
        BoundaryDirection, BoundaryId, BoundaryOperation, ProfileRevision,
    };
    use crate::executor::ledger::{BoundaryProgress, BoundaryProgressEntry};
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

    fn config() -> &'static TrackConfig {
        Box::leak(Box::new(TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        }))
    }

    fn operation() -> BoundaryOperation {
        BoundaryOperation {
            boundary_id: BoundaryId {
                profile_revision: profile_revision_for_config(config()),
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
            absorbed_exposure_qty: 0.0,
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
            filled_qty: 0.0,
            realized_pnl: 0.0,
            status: OrderStatus::New,
        }
    }

    fn recover_with(
        previous_state: &ExecutorState,
        live_orders: &[OrderObservation],
    ) -> RecoveryResolution {
        let rules = rules();
        let open_orders =
            CompleteOpenOrderSnapshot::from_complete_exchange_query(live_orders.to_vec());
        recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &Exposure(0.0),
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(previous_state),
            open_orders: &open_orders,
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
        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new());
        let recovery = recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &Exposure(0.0),
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(&ExecutorState::empty(Utc::now())),
            open_orders: &open_orders,
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected rebuilt state");
        };
        assert!(state.ledger_state.progress.is_empty());
    }

    #[test]
    fn recovery_does_not_rebuild_binding_from_pending_submit_hint_when_snapshot_binding_is_missing()
    {
        let rules = rules();
        let previous_state = ExecutorState::empty(Utc::now());
        let live_orders = vec![live_order("exchange-client", Side::Buy, 100.04, 1.004)];
        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(live_orders);

        let recovery = recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &Exposure(0.0),
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(&previous_state),
            open_orders: &open_orders,
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Anomaly {
            state,
            anomaly: RecoveryAnomaly::UnknownLiveOrder,
        } = recovery
        else {
            panic!("expected recovery to fail closed without snapshot binding");
        };
        assert!(state.bindings.is_empty());
    }

    #[test]
    fn recovery_reuses_existing_binding_by_snapshot_identity() {
        let rules = rules();
        let previous_binding = binding("expected-client", Side::Buy, 100.0, 1.0);
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state.bindings.push(previous_binding.clone());
        let live_orders = vec![live_order("expected-client", Side::Buy, 100.0, 1.0)];
        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(live_orders);

        let recovery = recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &Exposure(0.0),
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(&previous_state),
            open_orders: &open_orders,
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected recovery to reuse the existing binding");
        };
        assert_eq!(state.bindings.len(), 1);
        assert_eq!(state.bindings[0].binding_id, previous_binding.binding_id);
        assert_eq!(state.bindings[0].order_id.as_deref(), Some("live-order-1"));
    }

    #[test]
    fn recovery_terminalizes_cancel_pending_binding_missing_from_exchange_open_orders() {
        let rules = rules();
        let mut missing_binding = binding("missing-client", Side::Sell, 99.0, 1.0);
        missing_binding.status = BindingStatus::CancelPending;
        missing_binding.order_id = Some("missing-order".to_string());
        let mut live_binding = binding("live-client", Side::Buy, 100.0, 1.0);
        live_binding.status = BindingStatus::Working;
        live_binding.order_id = Some("stale-live-order".to_string());
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state.bindings.push(missing_binding);
        previous_state.bindings.push(live_binding);
        let live_orders = vec![live_order("live-client", Side::Buy, 100.0, 1.0)];
        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(live_orders);

        let recovery = recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &Exposure(0.0),
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(&previous_state),
            open_orders: &open_orders,
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected recovery to rebuild from exchange open orders");
        };
        assert_eq!(state.bindings[0].status, BindingStatus::Terminal);
        assert_eq!(state.bindings[1].status, BindingStatus::Working);
        assert_eq!(state.bindings[1].order_id.as_deref(), Some("live-order-1"));
    }

    #[test]
    fn recovery_absorbs_partial_live_order_progress_into_existing_binding() {
        let rules = rules();
        let previous_binding = binding("expected-client", Side::Buy, 100.0, 1.0);
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state = previous_state.ensure_revision(config(), Exposure(0.0));
        previous_state.bindings.push(previous_binding);
        let mut live_order = live_order("expected-client", Side::Buy, 100.0, 1.0);
        live_order.status = OrderStatus::PartiallyFilled;
        live_order.filled_qty = 0.4;
        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(vec![live_order]);

        let recovery = recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &Exposure(0.4),
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(&previous_state),
            open_orders: &open_orders,
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected recovery to rebuild partial live order progress");
        };
        assert!((state.bindings[0].absorbed_exposure_qty - 0.4).abs() < 1e-9);
        assert_eq!(state.ledger_state.ledger_anchor_exposure, Exposure(0.4));
        assert!(state.ledger_state.progress.is_empty());
    }

    #[test]
    fn recovery_reanchors_exposure_mismatch_after_complete_exchange_snapshot() {
        let rules = rules();
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state.ledger_state.profile_revision = ProfileRevision("rev-1".to_string());
        previous_state
            .ledger_state
            .progress
            .push(BoundaryProgressEntry {
                boundary_id: BoundaryId {
                    profile_revision: ProfileRevision("rev-1".to_string()),
                    lower_exposure_bp: 0,
                    upper_exposure_bp: 10_000,
                },
                progress: BoundaryProgress {
                    cumulative_up: 1.0,
                    cumulative_down: 0.0,
                },
            });

        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new());
        let recovery = recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &Exposure(-1.1),
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(&previous_state),
            open_orders: &open_orders,
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected complete exchange snapshot to reanchor executor ledger");
        };
        assert_eq!(state.recovery_anomaly, None);
        assert_eq!(state.ledger_state.ledger_anchor_exposure, Exposure(-1.1));
        assert!(state.ledger_state.progress.is_empty());
    }

    #[test]
    fn recovery_reanchors_exposure_drift_after_complete_exchange_snapshot() {
        let rules = rules();
        let current_exposure = Exposure(-4.2);
        let mut previous_state =
            ExecutorState::empty(Utc::now()).ensure_revision(config(), Exposure(2.0));
        previous_state.recovery_anomaly = Some(RecoveryAnomaly::ExpectedExposureMismatch);
        previous_state
            .bindings
            .push(binding("expected-client", Side::Buy, 100.0, 1.0));
        let live_orders = vec![live_order("expected-client", Side::Buy, 100.0, 1.0)];
        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(live_orders);

        let recovery = recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &current_exposure,
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(&previous_state),
            open_orders: &open_orders,
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected complete exchange snapshot to reanchor executor ledger");
        };
        assert_eq!(state.recovery_anomaly, None);
        assert_eq!(state.ledger_state.ledger_anchor_exposure, current_exposure);
        assert!(state.ledger_state.progress.is_empty());
        assert_eq!(state.bindings[0].status, BindingStatus::Working);
    }

    #[test]
    fn recovery_discards_invalid_boundary_progress_after_complete_exchange_snapshot() {
        let rules = rules();
        let mut previous_state =
            ExecutorState::empty(Utc::now()).ensure_revision(config(), Exposure(0.0));
        previous_state
            .ledger_state
            .progress
            .push(BoundaryProgressEntry {
                boundary_id: BoundaryId {
                    profile_revision: profile_revision_for_config(config()),
                    lower_exposure_bp: 0,
                    upper_exposure_bp: 10_000,
                },
                progress: BoundaryProgress {
                    cumulative_up: 1.2,
                    cumulative_down: 0.0,
                },
            });

        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new());
        let recovery = recover_working_orders(RecoveryInput {
            config: config(),
            current_exposure: &Exposure(1.2),
            desired_exposure: None,
            exchange_rules: &rules,
            previous_state: Some(&previous_state),
            open_orders: &open_orders,
            observed_at: Utc::now(),
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected complete exchange snapshot to discard invalid boundary progress");
        };
        assert_eq!(state.recovery_anomaly, None);
        assert_eq!(state.ledger_state.ledger_anchor_exposure, Exposure(1.2));
        assert!(state.ledger_state.progress.is_empty());
    }

    #[test]
    fn submit_recovery_proceeds_from_snapshot_binding_identity() {
        let rules = rules();
        let binding = binding("submit-1", Side::Buy, 100.0, 1.0);
        let target_request = binding.request.clone();
        let target_desired_exposure = binding.desired_exposure.clone();
        let target_recovery_token = SubmitRecoveryToken::from_binding(&binding);
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state.bindings.push(binding);

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            recovery_token: &target_recovery_token,
            current_exposure: &Exposure(0.0),
            live_order: None,
        });

        let SubmitRecoveryResolution::Proceed {
            request,
            desired_exposure,
        } = recovery.resolution
        else {
            panic!("expected submit recovery to reuse matching request");
        };
        assert_eq!(request.side, target_request.side);
        assert_eq!(request.price, target_request.price);
        assert_eq!(request.quantity, target_request.quantity);
        assert_eq!(request.reduce_only, target_request.reduce_only);
        assert_eq!(request.client_order_id, target_request.client_order_id);
        assert_eq!(desired_exposure, target_desired_exposure);
    }

    #[test]
    fn submit_recovery_without_stable_identity_waits_for_exchange_state() {
        let rules = rules();
        let existing_binding = binding("stale-client", Side::Buy, 100.0, 1.0);
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state.bindings.push(existing_binding.clone());
        let live_order = live_order("stale-client", Side::Buy, 100.0, 1.0);

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            recovery_token: &SubmitRecoveryToken::empty(),
            current_exposure: &Exposure(0.0),
            live_order: Some(&live_order),
        });

        assert!(matches!(
            recovery.resolution,
            SubmitRecoveryResolution::AwaitExchangeState
        ));
    }

    #[test]
    fn submit_recovery_supersedes_cancel_pending_binding_even_when_token_matches() {
        let rules = rules();
        let mut existing_binding = binding("stale-client", Side::Buy, 100.0, 1.0);
        existing_binding.status = BindingStatus::CancelPending;
        let recovery_token = SubmitRecoveryToken::from_binding(&existing_binding);
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state.bindings.push(existing_binding);

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            recovery_token: &recovery_token,
            current_exposure: &Exposure(0.0),
            live_order: None,
        });

        assert!(matches!(
            recovery.resolution,
            SubmitRecoveryResolution::Superseded { .. }
        ));
    }

    #[test]
    fn submit_recovery_supersedes_terminal_binding_even_when_token_matches() {
        let rules = rules();
        let mut existing_binding = binding("stale-client", Side::Buy, 100.0, 1.0);
        existing_binding.status = BindingStatus::Terminal;
        let recovery_token = SubmitRecoveryToken::from_binding(&existing_binding);
        let mut previous_state = ExecutorState::empty(Utc::now());
        previous_state.bindings.push(existing_binding);

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            recovery_token: &recovery_token,
            current_exposure: &Exposure(0.0),
            live_order: None,
        });

        assert!(matches!(
            recovery.resolution,
            SubmitRecoveryResolution::Superseded { .. }
        ));
    }
}
