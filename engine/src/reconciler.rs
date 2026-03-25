use grid_core::events::DomainEvent;
use grid_core::risk::{self, CapacityBudget, ExposureIntent, RiskDecision};
use grid_core::strategy::{self, BandStatus, OutOfBandPolicy};
use grid_core::types::{Exposure, Side};

use crate::execution_plan::{ExecutionAction, ExecutionPlan, is_meetable_minimum, round_to_step};
use crate::instance::{GridStatus, StrategyInstance};
use crate::ports::OrderRequest;

pub struct ReconcileResult {
    pub plan: ExecutionPlan,
    pub target_exposure: Exposure,
    pub new_status: Option<GridStatus>,
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

    let (target, new_status) = match &band {
        BandStatus::InBand { target } => (target.clone(), resolve_in_band_status(instance)),
        BandStatus::OutOfBand { policy, .. } => apply_out_of_band(instance, *policy),
    };

    let intent = ExposureIntent {
        current: instance.current_exposure.clone(),
        target: target.clone(),
        unit_notional: instance.config.notional_per_unit,
        realized_pnl_today: instance.risk_state.realized_pnl_today,
        unrealized_pnl: instance.risk_state.unrealized_pnl,
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

    // 带外 Freeze/Hold：保留离开带之前最后一个 target_exposure，并避免为了追赶 frozen target 而继续加风险。
    let would_increase_risk_out_of_band = matches!(
        band,
        BandStatus::OutOfBand {
            policy: OutOfBandPolicy::Freeze | OutOfBandPolicy::Hold,
            ..
        }
    ) && approved_target.0.abs()
        > instance.current_exposure.0.abs();

    if would_increase_risk_out_of_band {
        return ReconcileResult {
            plan: ExecutionPlan {
                actions: vec![ExecutionAction::NoOp],
                events,
            },
            target_exposure: approved_target,
            new_status,
        };
    }

    // 如果已有匹配的 pending_order（目标一致），避免重复下单。
    let pending_matches_target = instance
        .pending_order
        .as_ref()
        .map(|pending| (pending.target_exposure.0 - approved_target.0).abs() < f64::EPSILON)
        .unwrap_or(false);

    if pending_matches_target {
        return ReconcileResult {
            plan: ExecutionPlan {
                actions: vec![ExecutionAction::NoOp],
                events,
            },
            target_exposure: approved_target,
            new_status,
        };
    }

    let delta = instance.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return ReconcileResult {
            plan: ExecutionPlan {
                actions: vec![ExecutionAction::NoOp],
                events,
            },
            target_exposure: approved_target,
            new_status,
        };
    }

    events.push(DomainEvent::ExposureTargetChanged {
        from: instance.current_exposure.clone(),
        to: approved_target.clone(),
    });

    let side = Side::from_exposure(&delta).expect("non-zero delta must have side");
    let rules = &instance.exchange_rules;
    let price = round_to_step(price, rules.price_tick);
    let quantity = round_to_step(
        delta.0.abs() * instance.config.base_qty_per_unit(),
        rules.quantity_step,
    );

    let should_cancel_all = instance.pending_order.is_some();
    if !is_meetable_minimum(price, quantity, rules) {
        return ReconcileResult {
            plan: ExecutionPlan {
                actions: if should_cancel_all {
                    vec![ExecutionAction::CancelAll]
                } else {
                    vec![ExecutionAction::NoOp]
                },
                events,
            },
            target_exposure: approved_target,
            new_status,
        };
    }

    let request = OrderRequest {
        symbol: instance.symbol.clone(),
        side,
        price,
        quantity,
        client_order_id: format!("{}-reconcile", instance.id),
    };

    let actions = if should_cancel_all {
        vec![
            ExecutionAction::CancelAll,
            ExecutionAction::SubmitOrder {
                request,
                target_exposure: approved_target.clone(),
            },
        ]
    } else {
        vec![ExecutionAction::SubmitOrder {
            request,
            target_exposure: approved_target.clone(),
        }]
    };

    let plan = ExecutionPlan { actions, events };

    ReconcileResult {
        plan,
        target_exposure: approved_target,
        new_status,
    }
}

fn resolve_in_band_status(instance: &StrategyInstance) -> Option<GridStatus> {
    match instance.status {
        GridStatus::WaitingMarketData => Some(GridStatus::Active),
        GridStatus::Frozen | GridStatus::Holding => Some(GridStatus::Active),
        _ => None,
    }
}

fn apply_out_of_band(
    instance: &StrategyInstance,
    policy: OutOfBandPolicy,
) -> (Exposure, Option<GridStatus>) {
    let frozen_target = instance
        .target_exposure
        .clone()
        .unwrap_or_else(|| instance.current_exposure.clone());

    match policy {
        OutOfBandPolicy::Freeze => (frozen_target, Some(GridStatus::Frozen)),
        OutOfBandPolicy::Hold => (frozen_target, Some(GridStatus::Holding)),
        OutOfBandPolicy::ReduceOnly => (Exposure(0.0), Some(GridStatus::ReducingOnly)),
        OutOfBandPolicy::Terminate => (Exposure(0.0), Some(GridStatus::Terminated)),
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
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            grid_core::types::ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.1,
                min_qty: 0.0,
                min_notional: 0.0,
            },
        )
    }

    fn test_pending_order(target: Exposure) -> crate::instance::PendingOrder {
        crate::instance::PendingOrder {
            symbol: "BTCUSDT".to_string(),
            order_id: Some("order-1".to_string()),
            client_order_id: "client-1".to_string(),
            side: grid_core::types::Side::Buy,
            price: 90.0,
            quantity: 0.01,
            target_exposure: target,
            status: crate::ports::OrderStatus::New,
        }
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
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(0.0);
        instance.reference_price = Some(100.0);

        let result = reconcile(&instance, 100.0, &test_budget());
        assert!(!result.plan.has_actions());
    }

    #[test]
    fn reconcile_produces_event_when_exposure_changes() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;
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
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::Frozen));
    }

    #[test]
    fn reconcile_activates_on_first_price() {
        let instance = test_instance();
        assert_eq!(instance.status, GridStatus::WaitingMarketData);

        let result = reconcile(&instance, 100.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::Active));
    }

    #[test]
    fn reconcile_reactivates_after_reenter() {
        let mut instance = test_instance();
        instance.status = GridStatus::Frozen;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 100.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::Active));
    }

    #[test]
    fn reconcile_reduce_only_targets_zero() {
        let mut instance = test_instance();
        instance.config.out_of_band_policy = OutOfBandPolicy::ReduceOnly;
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::ReducingOnly));
        assert!((result.target_exposure.0).abs() < 0.001);
    }

    #[test]
    fn reconcile_terminate_targets_zero() {
        let mut instance = test_instance();
        instance.config.out_of_band_policy = OutOfBandPolicy::Terminate;
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(8.0);

        let result = reconcile(&instance, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::Terminated));
        assert!((result.target_exposure.0).abs() < 0.001);
    }

    #[test]
    fn reconcile_emits_risk_cap_event_when_budget_caps_target() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(0.0);

        let budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile(&instance, 90.0, &budget);

        assert_eq!(result.target_exposure, Exposure(4.0));
        assert!(result.plan.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied {
                intended,
                capped,
            } if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
    }

    #[test]
    fn reconcile_keeps_risk_cap_event_when_cap_matches_current_exposure() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(4.0);

        let budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile(&instance, 90.0, &budget);

        assert!(!result.plan.has_actions());
        assert_eq!(result.target_exposure, Exposure(4.0));
        assert!(result.plan.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied {
                intended,
                capped,
            } if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
        assert!(
            !result
                .plan
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ExposureTargetChanged { .. }))
        );
    }

    #[test]
    fn reconcile_uses_risk_state_pnl_to_cap_target() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(2.0);
        instance.risk_state.realized_pnl_today = -100.0;
        instance.risk_state.unrealized_pnl = -25.0;

        let result = reconcile(&instance, 90.0, &test_budget());

        assert_eq!(result.target_exposure, Exposure(0.0));
        assert!(result.plan.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied {
                intended,
                capped,
            } if *intended == Exposure(8.0) && *capped == Exposure(0.0)
        )));
    }

    #[test]
    fn reconcile_generates_submit_order_for_delta() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(0.0);
        instance.reference_price = Some(90.0);

        let result = reconcile(&instance, 90.0, &test_budget());

        assert!(matches!(
            result.plan.actions.as_slice(),
            [ExecutionAction::SubmitOrder { .. }]
        ));
    }

    #[test]
    fn reconcile_does_not_resubmit_when_pending_order_already_matches_target() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;
        instance.pending_order = Some(test_pending_order(Exposure(8.0)));

        let result = reconcile(&instance, 90.0, &test_budget());
        assert!(!result.plan.has_actions());
    }

    #[test]
    fn freeze_keeps_last_in_band_target_instead_of_current_exposure() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(4.0);
        instance.target_exposure = Some(Exposure(6.0));
        instance.config.out_of_band_policy = OutOfBandPolicy::Freeze;

        let result = reconcile(&instance, 85.0, &test_budget());

        assert_eq!(result.target_exposure.0, 6.0);
        assert!(!result.plan.has_actions());
    }

    #[test]
    fn reconcile_cancels_all_then_submits_when_pending_order_differs_from_target() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;
        instance.current_exposure = Exposure(0.0);
        instance.pending_order = Some(test_pending_order(Exposure(4.0)));

        let result = reconcile(&instance, 90.0, &test_budget());

        assert!(matches!(
            result.plan.actions.as_slice(),
            [
                ExecutionAction::CancelAll,
                ExecutionAction::SubmitOrder { .. }
            ]
        ));
    }

    #[test]
    fn reconcile_rounds_price_and_quantity_by_exchange_rules() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;

        let target = grid_core::strategy::target_exposure(99.09, &instance.config);
        instance.current_exposure = Exposure(target.0 - 0.8);
        instance.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.7,
            min_qty: 0.0,
            min_notional: 0.0,
        };

        let result = reconcile(&instance, 99.09, &test_budget());

        match result.plan.actions.as_slice() {
            [
                ExecutionAction::SubmitOrder {
                    request: req,
                    target_exposure,
                },
            ] => {
                assert!((req.price - 99.0).abs() < 1e-9);
                assert!((req.quantity - 2.8).abs() < 1e-9);
                assert_eq!(target_exposure, &target);
            }
            other => panic!("unexpected actions: {other:?}"),
        }
    }

    #[test]
    fn reconcile_does_not_submit_when_below_exchange_minimums() {
        let mut instance = test_instance();
        instance.status = GridStatus::Active;

        let target = grid_core::strategy::target_exposure(99.09, &instance.config);
        instance.current_exposure = Exposure(target.0 - 0.8);
        instance.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.7,
            min_qty: 10.0,
            min_notional: 999999.0,
        };

        let result = reconcile(&instance, 99.09, &test_budget());
        assert!(!result.plan.has_actions());
    }
}
