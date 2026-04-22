use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::executor::boundary::BoundaryOperation;
use crate::executor::ledger::BoundaryLedgerView;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    ManualOverride,
    Flatten,
    CatchUp,
    CurveMaker,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageReservation {
    operations: BTreeSet<BoundaryOperation>,
}

impl CoverageReservation {
    pub fn reserve(&mut self, operation: BoundaryOperation) {
        self.operations.insert(operation);
    }

    pub fn is_reserved(&self, operation: &BoundaryOperation) -> bool {
        self.operations.contains(operation)
    }
}

pub fn select_catch_up_operations(
    view: &BoundaryLedgerView,
    coverage: &CoverageReservation,
    exposure_epsilon: f64,
) -> Vec<BoundaryOperation> {
    view.operations
        .iter()
        .filter(|operation| operation.due)
        .filter(|operation| operation.remaining > exposure_epsilon)
        .filter(|operation| !coverage.is_reserved(&operation.operation))
        .map(|operation| operation.operation.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::executor::boundary::{
        BoundaryDirection, BoundaryId, BoundaryOperation, ProfileRevision,
    };
    use crate::executor::ledger::{BoundaryLedgerView, BoundaryOperationView};

    use super::*;

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
    fn catch_up_policy_selects_due_uncovered_operations_only() {
        let due = operation(0, 10_000, BoundaryDirection::Up);
        let future = operation(10_000, 20_000, BoundaryDirection::Up);
        let covered = operation(20_000, 30_000, BoundaryDirection::Up);
        let view = BoundaryLedgerView {
            operations: vec![
                BoundaryOperationView {
                    operation: due.clone(),
                    remaining: 1.0,
                    due: true,
                },
                BoundaryOperationView {
                    operation: future,
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: covered.clone(),
                    remaining: 1.0,
                    due: true,
                },
            ],
        };
        let mut coverage = CoverageReservation::default();
        coverage.reserve(covered);

        let selected = select_catch_up_operations(&view, &coverage, 1e-9);

        assert_eq!(selected, vec![due]);
    }
}
