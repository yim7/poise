use grid_core::events::DomainEvent;
use grid_core::risk::{self, CapacityBudget, ExposureIntent, RiskDecision};
use grid_core::strategy::{self, BandStatus, OutOfBandPolicy};
use grid_core::types::{Exposure, Side};

use crate::execution_plan::{ExecutionAction, is_meetable_minimum, round_to_step};
use crate::ports::OrderRequest;
use crate::runtime::{GridRuntime, GridStatus};

pub struct ReconcileResult {
    pub effects: Vec<ExecutionAction>,
    pub events: Vec<DomainEvent>,
    pub target_exposure: Exposure,
    pub new_status: Option<GridStatus>,
}

/// 纯函数：给定实例当前状态、价格和风控预算，返回执行计划。
///
/// 这是 engine 的核心协调逻辑：
/// 1. 调用 core::band_status 判断带内/带外
/// 2. 根据状态决定目标占用
/// 3. 调用 core::evaluate_risk 风控拦截
/// 4. 生成 effect 列表（数据，不是 IO）
pub fn reconcile(grid: &GridRuntime, price: f64, budget: &CapacityBudget) -> ReconcileResult {
    let band = strategy::band_status(price, &grid.config);

    let (target, new_status) = match &band {
        BandStatus::InBand { target } => (target.clone(), resolve_in_band_status(grid)),
        BandStatus::OutOfBand { policy, .. } => apply_out_of_band(grid, *policy),
    };

    let intent = ExposureIntent {
        current: grid.current_exposure.clone(),
        target: target.clone(),
        unit_notional: grid.config.notional_per_unit,
        realized_pnl_today: grid.risk_state.realized_pnl_today,
        unrealized_pnl: grid.risk_state.unrealized_pnl,
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
                effects: vec![ExecutionAction::NoOp],
                events: vec![DomainEvent::RiskDenied { reason }],
                target_exposure: grid.current_exposure.clone(),
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
        > grid.current_exposure.0.abs();

    if would_increase_risk_out_of_band {
        return ReconcileResult {
            effects: vec![ExecutionAction::NoOp],
            events,
            target_exposure: approved_target,
            new_status,
        };
    }

    // 如果已有匹配的 pending_order（目标一致），避免重复下单。
    let pending_matches_target = grid
        .pending_order
        .as_ref()
        .map(|pending| (pending.target_exposure.0 - approved_target.0).abs() < f64::EPSILON)
        .unwrap_or(false);

    if pending_matches_target {
        return ReconcileResult {
            effects: vec![ExecutionAction::NoOp],
            events,
            target_exposure: approved_target,
            new_status,
        };
    }

    let delta = grid.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return ReconcileResult {
            effects: vec![ExecutionAction::NoOp],
            events,
            target_exposure: approved_target,
            new_status,
        };
    }

    events.push(DomainEvent::ExposureTargetChanged {
        from: grid.current_exposure.clone(),
        to: approved_target.clone(),
    });

    let side = Side::from_exposure(&delta).expect("non-zero delta must have side");
    let rules = &grid.exchange_rules;
    let price = round_to_step(price, rules.price_tick);
    let quantity = round_to_step(
        delta.0.abs() * grid.config.base_qty_per_unit(),
        rules.quantity_step,
    );

    let should_cancel_all = grid.pending_order.is_some();
    if !is_meetable_minimum(price, quantity, rules) {
        return ReconcileResult {
            effects: if should_cancel_all {
                vec![ExecutionAction::CancelAll {
                    instrument: grid.instrument.clone(),
                }]
            } else {
                vec![ExecutionAction::NoOp]
            },
            events,
            target_exposure: approved_target,
            new_status,
        };
    }

    let request = OrderRequest {
        instrument: grid.instrument.clone(),
        side,
        price,
        quantity,
        client_order_id: format!("{}-reconcile", grid.id.as_str()),
    };

    let actions = if should_cancel_all {
        vec![
            ExecutionAction::CancelAll {
                instrument: grid.instrument.clone(),
            },
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

    ReconcileResult {
        effects: actions,
        events,
        target_exposure: approved_target,
        new_status,
    }
}

fn resolve_in_band_status(grid: &GridRuntime) -> Option<GridStatus> {
    match grid.status {
        GridStatus::WaitingMarketData => Some(GridStatus::Active),
        GridStatus::Frozen | GridStatus::Holding => Some(GridStatus::Active),
        _ => None,
    }
}

fn apply_out_of_band(
    grid: &GridRuntime,
    policy: OutOfBandPolicy,
) -> (Exposure, Option<GridStatus>) {
    let frozen_target = grid
        .target_exposure
        .clone()
        .unwrap_or_else(|| grid.current_exposure.clone());

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

    fn test_runtime() -> GridRuntime {
        GridRuntime::new(
            "test".into(),
            crate::grid::Instrument::new(crate::grid::Venue::Binance, "BTCUSDT"),
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

    fn test_pending_order(target: Exposure) -> crate::runtime::PendingOrder {
        crate::runtime::PendingOrder {
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
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.reference_price = Some(100.0);

        let result = reconcile(&grid, 100.0, &test_budget());
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
    }

    #[test]
    fn reconcile_produces_event_when_exposure_changes() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);

        let result = reconcile(&grid, 90.0, &test_budget());
        assert!((result.target_exposure.0 - 8.0).abs() < 0.001);
        assert!(
            result
                .events
                .iter()
                .any(|e| matches!(e, DomainEvent::ExposureTargetChanged { .. }))
        );
    }

    #[test]
    fn reconcile_freezes_when_out_of_band() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile(&grid, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::Frozen));
    }

    #[test]
    fn reconcile_activates_on_first_price() {
        let grid = test_runtime();
        assert_eq!(grid.status, GridStatus::WaitingMarketData);

        let result = reconcile(&grid, 100.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::Active));
    }

    #[test]
    fn reconcile_reactivates_after_reenter() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Frozen;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile(&grid, 100.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::Active));
    }

    #[test]
    fn reconcile_reduce_only_targets_zero() {
        let mut grid = test_runtime();
        grid.config.out_of_band_policy = OutOfBandPolicy::ReduceOnly;
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile(&grid, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::ReducingOnly));
        assert!((result.target_exposure.0).abs() < 0.001);
    }

    #[test]
    fn reconcile_terminate_targets_zero() {
        let mut grid = test_runtime();
        grid.config.out_of_band_policy = OutOfBandPolicy::Terminate;
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile(&grid, 85.0, &test_budget());
        assert_eq!(result.new_status, Some(GridStatus::Terminated));
        assert!((result.target_exposure.0).abs() < 0.001);
    }

    #[test]
    fn reconcile_emits_risk_cap_event_when_budget_caps_target() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);

        let budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile(&grid, 90.0, &budget);

        assert_eq!(result.target_exposure, Exposure(4.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied {
                intended,
                capped,
            } if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
    }

    #[test]
    fn reconcile_keeps_risk_cap_event_when_cap_matches_current_exposure() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(4.0);

        let budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile(&grid, 90.0, &budget);

        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
        assert_eq!(result.target_exposure, Exposure(4.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied {
                intended,
                capped,
            } if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ExposureTargetChanged { .. }))
        );
    }

    #[test]
    fn reconcile_uses_risk_state_pnl_to_cap_target() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(2.0);
        grid.risk_state.realized_pnl_today = -100.0;
        grid.risk_state.unrealized_pnl = -25.0;

        let result = reconcile(&grid, 90.0, &test_budget());

        assert_eq!(result.target_exposure, Exposure(0.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied {
                intended,
                capped,
            } if *intended == Exposure(8.0) && *capped == Exposure(0.0)
        )));
    }

    #[test]
    fn reconcile_generates_submit_order_for_delta() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.reference_price = Some(90.0);

        let result = reconcile(&grid, 90.0, &test_budget());

        assert!(matches!(
            result.effects.as_slice(),
            [ExecutionAction::SubmitOrder { .. }]
        ));
    }

    #[test]
    fn reconcile_does_not_resubmit_when_pending_order_already_matches_target() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.pending_order = Some(test_pending_order(Exposure(8.0)));

        let result = reconcile(&grid, 90.0, &test_budget());
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
    }

    #[test]
    fn freeze_keeps_last_in_band_target_instead_of_current_exposure() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(4.0);
        grid.target_exposure = Some(Exposure(6.0));
        grid.config.out_of_band_policy = OutOfBandPolicy::Freeze;

        let result = reconcile(&grid, 85.0, &test_budget());

        assert_eq!(result.target_exposure.0, 6.0);
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
    }

    #[test]
    fn reconcile_cancels_all_then_submits_when_pending_order_differs_from_target() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.pending_order = Some(test_pending_order(Exposure(4.0)));

        let result = reconcile(&grid, 90.0, &test_budget());

        assert!(matches!(
            result.effects.as_slice(),
            [
                ExecutionAction::CancelAll { .. },
                ExecutionAction::SubmitOrder { .. }
            ]
        ));
    }

    #[test]
    fn reconcile_rounds_price_and_quantity_by_exchange_rules() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;

        let target = grid_core::strategy::target_exposure(99.09, &grid.config);
        grid.current_exposure = Exposure(target.0 - 0.8);
        grid.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.7,
            min_qty: 0.0,
            min_notional: 0.0,
        };

        let result = reconcile(&grid, 99.09, &test_budget());

        match result.effects.as_slice() {
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
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;

        let target = grid_core::strategy::target_exposure(99.09, &grid.config);
        grid.current_exposure = Exposure(target.0 - 0.8);
        grid.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.7,
            min_qty: 10.0,
            min_notional: 999999.0,
        };

        let result = reconcile(&grid, 99.09, &test_budget());
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
    }
}
