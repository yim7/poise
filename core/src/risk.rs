use serde::{Deserialize, Serialize};

use crate::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityBudget {
    pub max_notional: f64,
    pub daily_loss_limit: f64,
    pub total_loss_limit: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LossGuardSnapshot {
    pub net_realized_pnl_today: f64,
    pub net_realized_pnl_cumulative: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExposureIntent {
    pub current: Exposure,
    pub target: Exposure,
    pub unit_notional: f64,
    pub loss_guard: LossGuardSnapshot,
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

    if !budget.daily_loss_limit.is_finite() || budget.daily_loss_limit <= 0.0 {
        return Err("daily_loss_limit must be finite and > 0".to_string());
    }

    if !budget.total_loss_limit.is_finite() || budget.total_loss_limit <= 0.0 {
        return Err("total_loss_limit must be finite and > 0".to_string());
    }

    Ok(())
}

/// 纯函数：评估风控。
pub fn evaluate_risk(intent: &ExposureIntent, budget: &CapacityBudget) -> RiskDecision {
    let daily_loss_amount =
        (-(intent.loss_guard.net_realized_pnl_today + intent.loss_guard.unrealized_pnl)).max(0.0);
    let total_loss_amount = (-(intent.loss_guard.net_realized_pnl_cumulative
        + intent.loss_guard.unrealized_pnl))
        .max(0.0);

    if daily_loss_amount >= budget.daily_loss_limit || total_loss_amount >= budget.total_loss_limit
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
            daily_loss_limit: 120.0,
            total_loss_limit: 500.0,
        }
    }

    fn empty_loss_guard() -> LossGuardSnapshot {
        LossGuardSnapshot {
            net_realized_pnl_today: 0.0,
            net_realized_pnl_cumulative: 0.0,
            unrealized_pnl: 0.0,
        }
    }

    #[test]
    fn allow_when_within_budget() {
        let intent = ExposureIntent {
            current: Exposure(0.0),
            target: Exposure(4.0),
            unit_notional: 375.0,
            loss_guard: empty_loss_guard(),
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
            loss_guard: empty_loss_guard(),
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
            loss_guard: empty_loss_guard(),
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
            loss_guard: empty_loss_guard(),
        };

        let decision = evaluate_risk(&intent, &budget());

        assert_eq!(decision, RiskDecision::Cap(Exposure(8.0)));
    }

    #[test]
    fn caps_to_zero_when_daily_loss_limit_is_breached_with_positive_limit() {
        let intent = ExposureIntent {
            current: Exposure(4.0),
            target: Exposure(8.0),
            unit_notional: 375.0,
            loss_guard: LossGuardSnapshot {
                net_realized_pnl_today: -90.0,
                net_realized_pnl_cumulative: -90.0,
                unrealized_pnl: -35.0,
            },
        };

        let decision = evaluate_risk(&intent, &budget());

        assert_eq!(decision, RiskDecision::Cap(Exposure(0.0)));
    }

    #[test]
    fn caps_to_zero_when_total_loss_limit_is_breached() {
        let intent = ExposureIntent {
            current: Exposure(4.0),
            target: Exposure(8.0),
            unit_notional: 375.0,
            loss_guard: LossGuardSnapshot {
                net_realized_pnl_today: -10.0,
                net_realized_pnl_cumulative: -480.0,
                unrealized_pnl: -30.0,
            },
        };
        let budget = CapacityBudget {
            daily_loss_limit: 200.0,
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
    fn validate_capacity_budget_rejects_non_positive_daily_loss_limit() {
        let error = validate_capacity_budget(&CapacityBudget {
            daily_loss_limit: 0.0,
            ..budget()
        })
        .unwrap_err();

        assert!(error.contains("daily_loss_limit"));
    }

    #[test]
    fn validate_capacity_budget_rejects_non_positive_total_loss_limit() {
        let error = validate_capacity_budget(&CapacityBudget {
            total_loss_limit: 0.0,
            ..budget()
        })
        .unwrap_err();

        assert!(error.contains("total_loss_limit"));
    }

    #[test]
    fn validate_capacity_budget_accepts_valid_budget() {
        assert!(validate_capacity_budget(&budget()).is_ok());
    }
}
