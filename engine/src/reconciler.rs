use grid_core::events::DomainEvent;
use grid_core::risk::{self, CapacityBudget, ExposureIntent, RiskDecision};
use grid_core::strategy::{self, BandStatus, OutOfBandPolicy};
use grid_core::types::Exposure;

use crate::execution_plan::{ExecutionAction, ExecutionPlan};
use crate::instance::{InstanceStatus, StrategyInstance};

pub struct ReconcileResult {
    pub plan: ExecutionPlan,
    pub target_exposure: Exposure,
    pub new_status: Option<InstanceStatus>,
}

/// 纯函数：给定实例当前状态、价格和风控预算，返回执行计划。
///
/// 这是 engine 的核心协调逻辑：
/// 1. 调用 core::band_status 判断带内/带外
/// 2. 根据状态决定目标占用
/// 3. 调用 core::evaluate_risk 风控拦截
/// 4. 生成 ExecutionPlan（数据，不是 IO）
pub fn reconcile(
    instance: &StrategyInstance,
    price: f64,
    budget: &CapacityBudget,
) -> ReconcileResult {
    let band = strategy::band_status(price, &instance.config);

    let (target, new_status) = match band {
        BandStatus::InBand { target } => (target, resolve_in_band_status(instance)),
        BandStatus::OutOfBand { policy, .. } => apply_out_of_band(instance, policy),
    };

    let intent = ExposureIntent {
        current: instance.current_exposure.clone(),
        target: target.clone(),
    };

    let decision = risk::evaluate_risk(&intent, budget);

    let (approved_target, mut events) = match decision {
        RiskDecision::Allow(t) => (t, vec![]),
        RiskDecision::Cap(t) => {
            let events = vec![DomainEvent::RiskCapApplied {
                intended: target.clone(),
                capped: t.clone(),
            }];
            (t, events)
        }
        RiskDecision::Deny { reason } => {
            return ReconcileResult {
                plan: ExecutionPlan::hold(reason),
                target_exposure: instance.current_exposure.clone(),
                new_status: None,
            };
        }
    };

    let delta = instance.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return ReconcileResult {
            plan: ExecutionPlan::noop(),
            target_exposure: approved_target,
            new_status,
        };
    }

    events.push(DomainEvent::ExposureTargetChanged {
        from: instance.current_exposure.clone(),
        to: approved_target.clone(),
    });

    // 执行计划中暂时只标记 NoOp，具体订单生成在后续阶段完善
    let plan = ExecutionPlan {
        actions: vec![ExecutionAction::NoOp],
        events,
    };

    ReconcileResult {
        plan,
        target_exposure: approved_target,
        new_status,
    }
}

fn resolve_in_band_status(instance: &StrategyInstance) -> Option<InstanceStatus> {
    match instance.status {
        InstanceStatus::WaitingMarketData => Some(InstanceStatus::Active),
        InstanceStatus::Frozen | InstanceStatus::Holding => Some(InstanceStatus::Active),
        _ => None,
    }
}

fn apply_out_of_band(
    instance: &StrategyInstance,
    policy: OutOfBandPolicy,
) -> (Exposure, Option<InstanceStatus>) {
    match policy {
        OutOfBandPolicy::Freeze => (
            instance.current_exposure.clone(),
            Some(InstanceStatus::Frozen),
        ),
        OutOfBandPolicy::Hold => (
            instance.current_exposure.clone(),
            Some(InstanceStatus::Holding),
        ),
        OutOfBandPolicy::ReduceOnly => (Exposure(0.0), Some(InstanceStatus::ReducingOnly)),
        OutOfBandPolicy::Terminate => (Exposure(0.0), Some(InstanceStatus::Terminated)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grid_core::strategy::*;

    fn test_instance() -> StrategyInstance {
        StrategyInstance::new(
            "test".into(),
            "BTCUSDT".into(),
            GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        )
    }

    fn test_budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 4.0,
        }
    }

    #[test]
    fn reconcile_noop_when_exposure_unchanged() {
        let mut instance = test_instance();
        instance.status = InstanceStatus::Active;
        instance.current_exposure = Exposure(0.0);
        instance.last_price = Some(100.0);

        let result = reconcile(&instance, 100.0, &test_budget());
        assert!(!result.plan.has_actions());
    }

    #[test]
    fn reconcile_produces_event_when_exposure_changes() {
        let mut instance = test_instance();
        instance.status = InstanceStatus::Active;
        instance.current_exposure = Exposure(0.0);

        let result = reconcile(&instance, 90.0, &test_budget());
        assert!((result.target_exposure.0 - 8.0).abs() < 0.001);
        assert!(
            result
                .plan
                .events
                .iter()
                .any(|e| matches!(e, DomainEvent::ExposureTargetChanged { .. }))
        );
    }

    #[test]
    fn reconcile_freezes_when_out_of_band() {
        let mut instance = test_instance();
        instance.status = InstanceStatus::Active;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(InstanceStatus::Frozen));
    }

    #[test]
    fn reconcile_activates_on_first_price() {
        let instance = test_instance();
        assert_eq!(instance.status, InstanceStatus::WaitingMarketData);

        let result = reconcile(&instance, 100.0, &test_budget());
        assert_eq!(result.new_status, Some(InstanceStatus::Active));
    }

    #[test]
    fn reconcile_reactivates_after_reenter() {
        let mut instance = test_instance();
        instance.status = InstanceStatus::Frozen;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 100.0, &test_budget());
        assert_eq!(result.new_status, Some(InstanceStatus::Active));
    }

    #[test]
    fn reconcile_reduce_only_targets_zero() {
        let mut instance = test_instance();
        instance.config.out_of_band_policy = OutOfBandPolicy::ReduceOnly;
        instance.status = InstanceStatus::Active;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(InstanceStatus::ReducingOnly));
        assert!((result.target_exposure.0).abs() < 0.001);
    }

    #[test]
    fn reconcile_terminate_targets_zero() {
        let mut instance = test_instance();
        instance.config.out_of_band_policy = OutOfBandPolicy::Terminate;
        instance.status = InstanceStatus::Active;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(InstanceStatus::Terminated));
        assert!((result.target_exposure.0).abs() < 0.001);
    }
}
