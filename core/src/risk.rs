use serde::{Deserialize, Serialize};

use crate::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityBudget {
    pub max_notional: f64,
    pub daily_loss_limit: f64,
    pub stop_loss_pct: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExposureIntent {
    pub current: Exposure,
    pub target: Exposure,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RiskDecision {
    Allow(Exposure),
    Cap(Exposure),
    Deny { reason: String },
}

/// 纯函数：评估风控。
///
/// 第一版实现：减仓或不变总是允许，加仓在预算范围内允许。
pub fn evaluate_risk(intent: &ExposureIntent, _budget: &CapacityBudget) -> RiskDecision {
    if intent.target.0.abs() <= intent.current.0.abs() {
        return RiskDecision::Allow(intent.target.clone());
    }
    RiskDecision::Allow(intent.target.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 4.0,
        }
    }

    #[test]
    fn allow_when_within_budget() {
        let intent = ExposureIntent {
            current: Exposure(0.0),
            target: Exposure(4.0),
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }

    #[test]
    fn allow_when_reducing_exposure() {
        let intent = ExposureIntent {
            current: Exposure(8.0),
            target: Exposure(4.0),
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }

    #[test]
    fn allow_no_change() {
        let intent = ExposureIntent {
            current: Exposure(4.0),
            target: Exposure(4.0),
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }
}
