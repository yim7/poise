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

pub fn select_curve_maker_operations(
    view: &BoundaryLedgerView,
    coverage: &CoverageReservation,
    exposure_epsilon: f64,
    levels_per_side: usize,
) -> Vec<BoundaryOperation> {
    let mut up = view
        .operations
        .iter()
        .filter(|operation| !operation.due)
        .filter(|operation| operation.remaining > exposure_epsilon)
        .filter(|operation| {
            operation.operation.direction == crate::executor::boundary::BoundaryDirection::Up
        })
        .filter(|operation| !coverage.is_reserved(&operation.operation))
        .map(|operation| operation.operation.clone())
        .take(levels_per_side)
        .collect::<Vec<_>>();
    let mut down = view
        .operations
        .iter()
        .rev()
        .filter(|operation| !operation.due)
        .filter(|operation| operation.remaining > exposure_epsilon)
        .filter(|operation| {
            operation.operation.direction == crate::executor::boundary::BoundaryDirection::Down
        })
        .filter(|operation| !coverage.is_reserved(&operation.operation))
        .map(|operation| operation.operation.clone())
        .take(levels_per_side)
        .collect::<Vec<_>>();

    up.append(&mut down);
    up
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

    #[test]
    fn curve_maker_policy_selects_nearest_future_operations_per_side() {
        let future_up_near = operation(0, 10_000, BoundaryDirection::Up);
        let future_up_far = operation(10_000, 20_000, BoundaryDirection::Up);
        let future_down_far = operation(-20_000, -10_000, BoundaryDirection::Down);
        let future_down_near = operation(-10_000, 0, BoundaryDirection::Down);
        let due = operation(20_000, 30_000, BoundaryDirection::Up);
        let view = BoundaryLedgerView {
            operations: vec![
                BoundaryOperationView {
                    operation: future_down_far,
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: future_down_near.clone(),
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: future_up_near.clone(),
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: future_up_far,
                    remaining: 1.0,
                    due: false,
                },
                BoundaryOperationView {
                    operation: due,
                    remaining: 1.0,
                    due: true,
                },
            ],
        };

        let selected =
            select_curve_maker_operations(&view, &CoverageReservation::default(), 1e-9, 1);

        assert_eq!(selected, vec![future_up_near, future_down_near]);
    }
}
