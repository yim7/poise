use poise_core::types::Exposure;
use serde::{Deserialize, Serialize};

use crate::executor::boundary::{
    BoundaryBlueprint, BoundaryDirection, BoundaryId, BoundaryOperation, ProfileRevision,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryProgress {
    pub cumulative_up: f64,
    pub cumulative_down: f64,
}

impl Default for BoundaryProgress {
    fn default() -> Self {
        Self {
            cumulative_up: 0.0,
            cumulative_down: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryProgressEntry {
    pub boundary_id: BoundaryId,
    pub progress: BoundaryProgress,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryLedgerState {
    pub profile_revision: ProfileRevision,
    pub ledger_anchor_exposure: Exposure,
    pub progress: Vec<BoundaryProgressEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundaryProgressView {
    pub effective_crossed_qty: f64,
    pub up_remaining: f64,
    pub down_remaining: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryLedgerError {
    pub boundary_id: BoundaryId,
    pub effective_crossed_qty: f64,
    pub step_size: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryOperationView {
    pub operation: BoundaryOperation,
    pub remaining: f64,
    pub due: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryLedgerView {
    pub operations: Vec<BoundaryOperationView>,
}

impl BoundaryLedgerState {
    pub fn try_progress_for(
        &self,
        boundary: &BoundaryBlueprint,
        exposure_epsilon: f64,
    ) -> Result<BoundaryProgressView, BoundaryLedgerError> {
        let progress = self
            .progress
            .iter()
            .find(|entry| entry.boundary_id == boundary.id)
            .map(|entry| entry.progress.clone())
            .unwrap_or_default();
        let anchor_crossed = anchor_crossed_qty(boundary, self.ledger_anchor_exposure.0);
        let raw_effective_crossed_qty =
            anchor_crossed + progress.cumulative_up - progress.cumulative_down;
        let epsilon = exposure_epsilon.max(f64::EPSILON);
        if raw_effective_crossed_qty < -epsilon
            || raw_effective_crossed_qty > boundary.step_size + epsilon
        {
            return Err(BoundaryLedgerError {
                boundary_id: boundary.id.clone(),
                effective_crossed_qty: raw_effective_crossed_qty,
                step_size: boundary.step_size,
            });
        }
        let effective_crossed_qty = raw_effective_crossed_qty.clamp(0.0, boundary.step_size);

        Ok(BoundaryProgressView {
            effective_crossed_qty,
            up_remaining: (boundary.step_size - effective_crossed_qty).max(0.0),
            down_remaining: effective_crossed_qty.max(0.0),
        })
    }

    pub fn expected_exposure(&self) -> Exposure {
        let progress_delta = self
            .progress
            .iter()
            .map(|entry| entry.progress.cumulative_up - entry.progress.cumulative_down)
            .sum::<f64>();
        Exposure(self.ledger_anchor_exposure.0 + progress_delta)
    }

    pub fn has_unexplained_exposure_drift(
        &self,
        current_exposure: &Exposure,
        active_binding_exposure_budget: f64,
        exposure_epsilon: f64,
    ) -> bool {
        let drift = (current_exposure.0 - self.expected_exposure().0).abs();
        let explainable_budget = active_binding_exposure_budget.max(0.0);
        drift > explainable_budget + exposure_epsilon.max(f64::EPSILON)
    }
}

impl BoundaryLedgerView {
    pub fn try_from_boundaries(
        boundaries: &[BoundaryBlueprint],
        state: &BoundaryLedgerState,
        spot_target: Exposure,
        exposure_epsilon: f64,
    ) -> Result<Self, BoundaryLedgerError> {
        let mut operations = Vec::new();
        for boundary in boundaries {
            let progress = state.try_progress_for(boundary, exposure_epsilon)?;
            operations.push(BoundaryOperationView {
                operation: BoundaryOperation {
                    boundary_id: boundary.id.clone(),
                    direction: BoundaryDirection::Up,
                },
                remaining: progress.up_remaining,
                due: spot_target.0 >= boundary.upper_exposure.0 - exposure_epsilon
                    && progress.up_remaining > exposure_epsilon,
            });
            operations.push(BoundaryOperationView {
                operation: BoundaryOperation {
                    boundary_id: boundary.id.clone(),
                    direction: BoundaryDirection::Down,
                },
                remaining: progress.down_remaining,
                due: spot_target.0 <= boundary.lower_exposure.0 + exposure_epsilon
                    && progress.down_remaining > exposure_epsilon,
            });
        }

        Ok(Self { operations })
    }

    #[allow(dead_code)]
    pub fn is_due(&self, operation: &BoundaryOperation) -> bool {
        self.operations
            .iter()
            .any(|view| view.operation == *operation && view.due)
    }
}

fn anchor_crossed_qty(boundary: &BoundaryBlueprint, anchor_exposure: f64) -> f64 {
    if anchor_exposure <= boundary.lower_exposure.0 {
        0.0
    } else if anchor_exposure >= boundary.upper_exposure.0 {
        boundary.step_size
    } else {
        anchor_exposure - boundary.lower_exposure.0
    }
}

#[cfg(test)]
mod tests {
    use poise_core::types::Exposure;

    use super::*;

    fn boundary(lower: f64, upper: f64) -> BoundaryBlueprint {
        BoundaryBlueprint {
            id: BoundaryId {
                profile_revision: ProfileRevision("rev-1".to_string()),
                lower_exposure_bp: (lower * 10_000.0).round() as i64,
                upper_exposure_bp: (upper * 10_000.0).round() as i64,
            },
            lower_exposure: Exposure(lower),
            upper_exposure: Exposure(upper),
            trigger_price: 100.0,
            step_size: upper - lower,
        }
    }

    #[test]
    fn boundary_progress_derives_remaining_from_anchor_and_cumulative_deltas() {
        let boundary = boundary(1.0, 2.0);
        let state = BoundaryLedgerState {
            profile_revision: ProfileRevision("rev-1".to_string()),
            ledger_anchor_exposure: Exposure(1.0),
            progress: vec![BoundaryProgressEntry {
                boundary_id: boundary.id.clone(),
                progress: BoundaryProgress {
                    cumulative_up: 0.6,
                    cumulative_down: 0.2,
                },
            }],
        };

        let progress = state.try_progress_for(&boundary, 1e-9).unwrap();

        assert!((progress.effective_crossed_qty - 0.4).abs() < 1e-9);
        assert!((progress.up_remaining - 0.6).abs() < 1e-9);
        assert!((progress.down_remaining - 0.4).abs() < 1e-9);
    }

    #[test]
    fn boundary_progress_includes_anchor_when_anchor_starts_above_boundary() {
        let boundary = boundary(1.0, 2.0);
        let state = BoundaryLedgerState {
            profile_revision: ProfileRevision("rev-1".to_string()),
            ledger_anchor_exposure: Exposure(2.0),
            progress: vec![BoundaryProgressEntry {
                boundary_id: boundary.id.clone(),
                progress: BoundaryProgress {
                    cumulative_up: 0.0,
                    cumulative_down: 0.4,
                },
            }],
        };

        let progress = state.try_progress_for(&boundary, 1e-9).unwrap();

        assert!((progress.effective_crossed_qty - 0.6).abs() < 1e-9);
        assert!((progress.up_remaining - 0.4).abs() < 1e-9);
        assert!((progress.down_remaining - 0.6).abs() < 1e-9);
    }

    #[test]
    fn boundary_progress_includes_anchor_when_anchor_starts_inside_boundary() {
        let boundary = boundary(1.0, 2.0);
        let state = BoundaryLedgerState {
            profile_revision: ProfileRevision("rev-1".to_string()),
            ledger_anchor_exposure: Exposure(1.4),
            progress: vec![BoundaryProgressEntry {
                boundary_id: boundary.id.clone(),
                progress: BoundaryProgress {
                    cumulative_up: 0.2,
                    cumulative_down: 0.1,
                },
            }],
        };

        let progress = state.try_progress_for(&boundary, 1e-9).unwrap();

        assert!((progress.effective_crossed_qty - 0.5).abs() < 1e-9);
        assert!((progress.up_remaining - 0.5).abs() < 1e-9);
        assert!((progress.down_remaining - 0.5).abs() < 1e-9);
    }

    #[test]
    fn boundary_progress_rejects_out_of_range_net_progress() {
        let boundary = boundary(1.0, 2.0);
        let state = BoundaryLedgerState {
            profile_revision: ProfileRevision("rev-1".to_string()),
            ledger_anchor_exposure: Exposure(1.0),
            progress: vec![BoundaryProgressEntry {
                boundary_id: boundary.id.clone(),
                progress: BoundaryProgress {
                    cumulative_up: 1.2,
                    cumulative_down: 0.0,
                },
            }],
        };

        assert!(state.try_progress_for(&boundary, 1e-9).is_err());
    }

    #[test]
    fn due_direction_flips_when_spot_target_crosses_boundary() {
        let boundary = boundary(1.0, 2.0);
        let state = BoundaryLedgerState {
            profile_revision: ProfileRevision("rev-1".to_string()),
            ledger_anchor_exposure: Exposure(1.0),
            progress: Vec::new(),
        };

        let up_view = BoundaryLedgerView::try_from_boundaries(
            std::slice::from_ref(&boundary),
            &state,
            Exposure(2.0),
            1e-9,
        )
        .unwrap();
        let down_view = BoundaryLedgerView::try_from_boundaries(
            std::slice::from_ref(&boundary),
            &BoundaryLedgerState {
                ledger_anchor_exposure: Exposure(2.0),
                ..state
            },
            Exposure(1.0),
            1e-9,
        )
        .unwrap();

        assert!(up_view.is_due(&BoundaryOperation {
            boundary_id: boundary.id.clone(),
            direction: BoundaryDirection::Up,
        }));
        assert!(down_view.is_due(&BoundaryOperation {
            boundary_id: boundary.id,
            direction: BoundaryDirection::Down,
        }));
    }
}
