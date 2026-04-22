use serde::{Deserialize, Serialize};

use crate::executor::boundary::BoundaryOperation;
use crate::executor::policy::PolicyKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveOrderBinding {
    pub binding_id: String,
    pub proposal_key: BindingProposalKey,
    pub status: BindingStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingStatus {
    SubmitPending,
    Working,
    CancelPending,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingProposal {
    pub policy: PolicyKind,
    pub operations: Vec<BoundaryOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BindingProposalKey {
    pub policy: PolicyKind,
    pub operations: Vec<BoundaryOperation>,
}

impl BindingProposal {
    pub fn proposal_key(&self) -> BindingProposalKey {
        BindingProposalKey {
            policy: self.policy,
            operations: self.operations.clone(),
        }
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
            status: BindingStatus::Working,
        };
        assert_eq!(binding.status, BindingStatus::Working);
    }
}
