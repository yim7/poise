use serde::{Deserialize, Serialize};

use crate::executor::boundary::{BoundaryDirection, BoundaryOperation};
use crate::executor::policy::PolicyKind;
use crate::ports::OrderRequest;
use crate::price_gate::SubmitPurpose;
use poise_core::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveOrderBinding {
    pub binding_id: String,
    pub proposal_key: BindingProposalKey,
    pub allocations: Vec<BindingOperationAllocation>,
    #[serde(default)]
    pub absorbed_exposure_qty: f64,
    pub request: OrderRequest,
    pub desired_exposure: Exposure,
    pub submit_purpose: SubmitPurpose,
    pub order_id: Option<String>,
    pub status: BindingStatus,
    pub policy_state: BindingPolicyState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingStatus {
    SubmitPending,
    Working,
    CancelPending,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingPolicyState {
    Stateless,
    CurveMaker {
        due_grace_started_at: Option<chrono::DateTime<chrono::Utc>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubmitRecoveryToken(String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SubmitRecoveryPayload {
    submit_attempt_id: String,
    binding_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubmitRecoveryBinding {
    pub submit_attempt_id: String,
    pub binding_id: String,
}

impl SubmitRecoveryToken {
    pub fn empty() -> Self {
        Self(String::new())
    }

    pub(crate) fn from_binding(binding: &LiveOrderBinding) -> Self {
        let payload = SubmitRecoveryPayload {
            submit_attempt_id: binding.request.client_order_id.clone(),
            binding_id: binding.binding_id.clone(),
        };
        Self(
            serde_json::to_string(&payload)
                .expect("submit recovery payload should always serialize"),
        )
    }

    pub(crate) fn decode(&self) -> Option<SubmitRecoveryBinding> {
        if self.0.is_empty() {
            return None;
        }

        let payload = serde_json::from_str::<SubmitRecoveryPayload>(&self.0).ok()?;
        Some(SubmitRecoveryBinding {
            submit_attempt_id: payload.submit_attempt_id,
            binding_id: payload.binding_id,
        })
    }

    pub(crate) fn matches_submission_identity(&self, other: &Self) -> bool {
        match (self.decode(), other.decode()) {
            (Some(left), Some(right)) => left.submit_attempt_id == right.submit_attempt_id,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingProposal {
    pub policy: PolicyKind,
    pub operations: Vec<BoundaryOperation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingOperationAllocation {
    pub operation: BoundaryOperation,
    pub exposure_qty: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BindingProposalKey {
    pub policy: PolicyKind,
    pub operations: Vec<BoundaryOperation>,
}

pub fn binding_is_active(binding: &LiveOrderBinding) -> bool {
    matches!(
        binding.status,
        BindingStatus::SubmitPending | BindingStatus::Working | BindingStatus::CancelPending
    )
}

pub fn active_binding_exposure_budget(bindings: &[LiveOrderBinding]) -> f64 {
    bindings
        .iter()
        .filter(|binding| binding_is_active(binding))
        .map(|binding| {
            (binding.total_allocated_exposure_qty() - binding.absorbed_exposure_qty).max(0.0)
        })
        .sum()
}

impl BindingProposal {
    pub fn proposal_key(&self) -> BindingProposalKey {
        BindingProposalKey {
            policy: self.policy,
            operations: self.operations.clone(),
        }
    }
}

impl LiveOrderBinding {
    pub fn total_allocated_exposure_qty(&self) -> f64 {
        self.allocations
            .iter()
            .map(|allocation| allocation.exposure_qty)
            .sum()
    }

    pub fn is_active(&self) -> bool {
        binding_is_active(self)
    }

    pub fn is_submit_pending(&self) -> bool {
        self.status == BindingStatus::SubmitPending
    }

    pub fn policy(&self) -> PolicyKind {
        self.proposal_key.policy
    }

    pub fn is_passive_execution(&self) -> bool {
        self.proposal_key.policy == PolicyKind::CurveMaker
    }

    pub fn increases_inventory(&self) -> bool {
        let signed_exposure = self
            .allocations
            .iter()
            .map(|allocation| match allocation.operation.direction {
                BoundaryDirection::Up => allocation.exposure_qty,
                BoundaryDirection::Down => -allocation.exposure_qty,
            })
            .sum::<f64>();
        signed_exposure >= 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::boundary::{
        BoundaryDirection, BoundaryId, BoundaryOperation, ProfileRevision,
    };

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

    #[test]
    fn binding_proposal_key_is_policy_plus_ordered_operations() {
        let first = operation(0, 10_000, BoundaryDirection::Up);
        let second = operation(10_000, 20_000, BoundaryDirection::Up);
        let proposal = BindingProposal {
            policy: PolicyKind::CatchUp,
            operations: vec![first.clone(), second.clone()],
        };

        assert_eq!(
            proposal.proposal_key(),
            BindingProposalKey {
                policy: PolicyKind::CatchUp,
                operations: vec![first, second],
            }
        );

        let binding = LiveOrderBinding {
            binding_id: "binding-1".to_string(),
            proposal_key: proposal.proposal_key(),
            allocations: Vec::new(),
            absorbed_exposure_qty: 0.0,
            request: OrderRequest {
                instrument: crate::track::Instrument::new(crate::track::Venue::Binance, "BTCUSDT"),
                side: poise_core::types::Side::Buy,
                price: 100.0,
                quantity: 1.0,
                client_order_id: "client-1".to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(1.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            order_id: None,
            status: BindingStatus::Working,
            policy_state: BindingPolicyState::Stateless,
        };
        assert_eq!(binding.status, BindingStatus::Working);
    }

    #[test]
    fn submit_recovery_token_identifies_submit_attempt_not_proposal() {
        let operation = operation(0, 10_000, BoundaryDirection::Up);
        let proposal = BindingProposal {
            policy: PolicyKind::CurveMaker,
            operations: vec![operation.clone()],
        };
        let proposal_key = proposal.proposal_key();
        let binding = |client_order_id: &str| LiveOrderBinding {
            binding_id: "curve-maker:0:1:up".to_string(),
            proposal_key: proposal_key.clone(),
            allocations: vec![BindingOperationAllocation {
                operation: operation.clone(),
                exposure_qty: 1.0,
            }],
            absorbed_exposure_qty: 0.0,
            request: OrderRequest {
                instrument: crate::track::Instrument::new(crate::track::Venue::Binance, "BTCUSDT"),
                side: poise_core::types::Side::Buy,
                price: 100.0,
                quantity: 1.0,
                client_order_id: client_order_id.to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(1.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            order_id: None,
            status: BindingStatus::SubmitPending,
            policy_state: BindingPolicyState::Stateless,
        };

        let stale_attempt = SubmitRecoveryToken::from_binding(&binding("submit-1"));
        let later_attempt_for_same_operation =
            SubmitRecoveryToken::from_binding(&binding("submit-2"));

        assert!(
            !stale_attempt.matches_submission_identity(&later_attempt_for_same_operation),
            "same boundary proposal must not let a stale submit effect own a later submit instance"
        );
    }

    #[test]
    fn submit_recovery_token_only_serializes_submit_identity() {
        let binding = LiveOrderBinding {
            binding_id: "binding-instance-1".to_string(),
            proposal_key: BindingProposalKey {
                policy: PolicyKind::CurveMaker,
                operations: vec![operation(0, 10_000, BoundaryDirection::Up)],
            },
            allocations: vec![BindingOperationAllocation {
                operation: operation(0, 10_000, BoundaryDirection::Up),
                exposure_qty: 1.0,
            }],
            absorbed_exposure_qty: 0.0,
            request: OrderRequest {
                instrument: crate::track::Instrument::new(crate::track::Venue::Binance, "BTCUSDT"),
                side: poise_core::types::Side::Buy,
                price: 100.0,
                quantity: 1.0,
                client_order_id: "submit-1".to_string(),
                reduce_only: false,
            },
            desired_exposure: Exposure(1.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            order_id: None,
            status: BindingStatus::SubmitPending,
            policy_state: BindingPolicyState::CurveMaker {
                due_grace_started_at: None,
            },
        };

        let token = SubmitRecoveryToken::from_binding(&binding);
        let payload: serde_json::Value = serde_json::from_str(&token.0).unwrap();

        assert_eq!(
            payload
                .get("binding_id")
                .and_then(serde_json::Value::as_str),
            Some("binding-instance-1")
        );
        assert_eq!(
            payload
                .get("submit_attempt_id")
                .and_then(serde_json::Value::as_str),
            Some("submit-1")
        );
        assert!(payload.get("proposal_key").is_none());
        assert!(payload.get("allocations").is_none());
        assert!(payload.get("policy_state").is_none());
    }
}
