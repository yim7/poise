use poise_core::events::ExecutionGateReason;
use poise_core::types::Exposure;
use serde::{Deserialize, Serialize};

use crate::price_gate::PriceExecutionBlockReason;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AccountCapacityGateState {
    pub available_notional: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountCapacityGateInput {
    pub current: Exposure,
    pub approved_target: Exposure,
    pub unit_notional: f64,
    pub available_notional: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum ExecutionGateDecision {
    #[default]
    Open,
    NoSubmit {
        reason: ExecutionGateReason,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ExecutionGateState {
    #[serde(default)]
    pub account_capacity: AccountCapacityGateState,
    #[serde(default)]
    pub price_execution_block_reason: Option<PriceExecutionBlockReason>,
    #[serde(default)]
    pub last_decision: ExecutionGateDecision,
}

impl ExecutionGateState {
    pub fn open() -> Self {
        Self::default()
    }
}

pub struct AccountCapacityGate;

impl AccountCapacityGate {
    pub fn evaluate(input: AccountCapacityGateInput) -> ExecutionGateDecision {
        let Some(available_notional) = input.available_notional else {
            return ExecutionGateDecision::Open;
        };

        let increase_units = added_risk_units(input.current.0, input.approved_target.0);
        let required_notional = increase_units * input.unit_notional;

        if required_notional > available_notional + f64::EPSILON {
            return ExecutionGateDecision::NoSubmit {
                reason: ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional,
                    available_notional,
                },
            };
        }

        ExecutionGateDecision::Open
    }
}

fn added_risk_units(current: f64, approved_target: f64) -> f64 {
    let crosses_zero =
        (current > 0.0 && approved_target < 0.0) || (current < 0.0 && approved_target > 0.0);
    if crosses_zero {
        approved_target.abs()
    } else {
        (approved_target.abs() - current.abs()).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(
        current: f64,
        approved_target: f64,
        unit_notional: f64,
        available_notional: f64,
    ) -> ExecutionGateDecision {
        AccountCapacityGate::evaluate(AccountCapacityGateInput {
            current: Exposure(current),
            approved_target: Exposure(approved_target),
            unit_notional,
            available_notional: Some(available_notional),
        })
    }

    #[test]
    fn account_capacity_gate_blocks_increase_without_risk_outcome() {
        let decision = evaluate(2.0, 6.0, 375.0, 1_000.0);

        assert_eq!(
            decision,
            ExecutionGateDecision::NoSubmit {
                reason: ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 1_500.0,
                    available_notional: 1_000.0,
                },
            },
        );
    }

    #[test]
    fn account_capacity_gate_counts_new_short_risk_after_flattening_long() {
        assert_eq!(
            evaluate(3.0, -0.5, 100.0, 49.0),
            ExecutionGateDecision::NoSubmit {
                reason: ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 50.0,
                    available_notional: 49.0,
                },
            },
        );
        assert_eq!(
            evaluate(3.0, -0.5, 100.0, 50.0),
            ExecutionGateDecision::Open,
        );
    }

    #[test]
    fn account_capacity_gate_counts_new_long_risk_after_flattening_short() {
        assert_eq!(
            evaluate(-3.0, 0.5, 100.0, 49.0),
            ExecutionGateDecision::NoSubmit {
                reason: ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 50.0,
                    available_notional: 49.0,
                },
            },
        );
        assert_eq!(
            evaluate(-3.0, 0.5, 100.0, 50.0),
            ExecutionGateDecision::Open,
        );
    }

    #[test]
    fn account_capacity_gate_does_not_charge_for_reducing_or_flattening_risk() {
        for (current, approved_target) in [(3.0, 1.0), (-3.0, -1.0), (3.0, 0.0), (-3.0, 0.0)] {
            assert_eq!(
                evaluate(current, approved_target, 100.0, 0.0),
                ExecutionGateDecision::Open,
                "current={current}, approved_target={approved_target}",
            );
        }
    }

    #[test]
    fn account_capacity_gate_counts_full_target_when_starting_flat() {
        assert_eq!(
            evaluate(0.0, 0.5, 100.0, 49.0),
            ExecutionGateDecision::NoSubmit {
                reason: ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 50.0,
                    available_notional: 49.0,
                },
            },
        );
        assert_eq!(
            evaluate(0.0, -0.5, 100.0, 50.0),
            ExecutionGateDecision::Open,
        );
    }
}
