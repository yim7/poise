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
    pub unit_notional: f64,
    pub realized_pnl_today: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RiskDecision {
    Allow(Exposure),
    Cap(Exposure),
    Deny { reason: String },
}

pub fn validate_capacity_budget(budget: &CapacityBudget) -> Result<(), String> {
    if !budget.max_notional.is_finite() || budget.max_notional <= 0.0 {
        return Err("max_notional must be finite and > 0".to_string());
    }

    if !budget.daily_loss_limit.is_finite() || budget.daily_loss_limit >= 0.0 {
        return Err("daily_loss_limit must be finite and < 0".to_string());
    }

    if !budget.stop_loss_pct.is_finite() || budget.stop_loss_pct <= 0.0 {
        return Err("stop_loss_pct must be finite and > 0".to_string());
    }

    Ok(())
}

/// 纯函数：评估风控。
pub fn evaluate_risk(intent: &ExposureIntent, budget: &CapacityBudget) -> RiskDecision {
    let total_pnl = intent.realized_pnl_today + intent.unrealized_pnl;

    if total_pnl <= budget.daily_loss_limit {
        return RiskDecision::Cap(Exposure(0.0));
    }

    if budget.max_notional > 0.0
        && ((-total_pnl / budget.max_notional) * 100.0) >= budget.stop_loss_pct
    {
        return RiskDecision::Cap(Exposure(0.0));
    }

    if budget.max_notional > 0.0 && intent.unit_notional > 0.0 {
        let max_abs_exposure = budget.max_notional / intent.unit_notional;
        if intent.target.0.abs() > max_abs_exposure {
            return RiskDecision::Cap(Exposure(intent.target.0.signum() * max_abs_exposure));
        }
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
            unit_notional: 375.0,
            realized_pnl_today: 0.0,
            unrealized_pnl: 0.0,
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }

    #[test]
    fn allow_when_reducing_exposure() {
        let intent = ExposureIntent {
            current: Exposure(8.0),
            target: Exposure(4.0),
            unit_notional: 375.0,
            realized_pnl_today: 0.0,
            unrealized_pnl: 0.0,
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }

    #[test]
    fn allow_no_change() {
        let intent = ExposureIntent {
            current: Exposure(4.0),
            target: Exposure(4.0),
            unit_notional: 375.0,
            realized_pnl_today: 0.0,
            unrealized_pnl: 0.0,
        };
        let decision = evaluate_risk(&intent, &budget());
        assert!(matches!(decision, RiskDecision::Allow(_)));
    }

    #[test]
    fn caps_target_when_max_notional_is_exceeded() {
        let intent = ExposureIntent {
            current: Exposure(0.0),
            target: Exposure(10.0),
            unit_notional: 375.0,
            realized_pnl_today: 0.0,
            unrealized_pnl: 0.0,
        };

        let decision = evaluate_risk(&intent, &budget());

        assert_eq!(decision, RiskDecision::Cap(Exposure(8.0)));
    }

    #[test]
    fn caps_to_zero_when_daily_loss_limit_is_breached() {
        let intent = ExposureIntent {
            current: Exposure(4.0),
            target: Exposure(8.0),
            unit_notional: 375.0,
            realized_pnl_today: -100.0,
            unrealized_pnl: -25.0,
        };

        let decision = evaluate_risk(&intent, &budget());

        assert_eq!(decision, RiskDecision::Cap(Exposure(0.0)));
    }

    #[test]
    fn caps_to_zero_when_stop_loss_pct_is_breached() {
        let intent = ExposureIntent {
            current: Exposure(4.0),
            target: Exposure(8.0),
            unit_notional: 375.0,
            realized_pnl_today: -50.0,
            unrealized_pnl: -70.0,
        };
        let budget = CapacityBudget {
            daily_loss_limit: -200.0,
            ..budget()
        };

        let decision = evaluate_risk(&intent, &budget);

        assert_eq!(decision, RiskDecision::Cap(Exposure(0.0)));
    }

    #[test]
    fn validate_capacity_budget_rejects_non_positive_max_notional() {
        let error = validate_capacity_budget(&CapacityBudget {
            max_notional: 0.0,
            ..budget()
        })
        .unwrap_err();

        assert!(error.contains("max_notional"));
    }

    #[test]
    fn validate_capacity_budget_rejects_non_negative_daily_loss_limit() {
        let error = validate_capacity_budget(&CapacityBudget {
            daily_loss_limit: 0.0,
            ..budget()
        })
        .unwrap_err();

        assert!(error.contains("daily_loss_limit"));
    }

    #[test]
    fn validate_capacity_budget_rejects_non_positive_stop_loss_pct() {
        let error = validate_capacity_budget(&CapacityBudget {
            stop_loss_pct: 0.0,
            ..budget()
        })
        .unwrap_err();

        assert!(error.contains("stop_loss_pct"));
    }

    #[test]
    fn validate_capacity_budget_accepts_valid_budget() {
        assert!(validate_capacity_budget(&budget()).is_ok());
    }
}
