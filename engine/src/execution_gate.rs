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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExecutionGateDecision {
    Open,
    NoSubmit { reason: ExecutionGateReason },
}

impl Default for ExecutionGateDecision {
    fn default() -> Self {
        Self::Open
    }
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

        let increase_units = (input.approved_target.0.abs() - input.current.0.abs()).max(0.0);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_capacity_gate_blocks_increase_without_risk_outcome() {
        let decision = AccountCapacityGate::evaluate(AccountCapacityGateInput {
            current: Exposure(2.0),
            approved_target: Exposure(6.0),
            unit_notional: 375.0,
            available_notional: Some(1_000.0),
        });

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
}
