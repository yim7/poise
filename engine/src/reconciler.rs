use grid_core::events::{DomainEvent, ReplacementGateReason};
use grid_core::risk::{self, ExposureIntent, RiskDecision};
use grid_core::strategy::{self, BandStatus, OutOfBandPolicy};
use grid_core::types::{Exposure, Side};

use crate::execution_plan::{ExecutionAction, is_meetable_minimum, round_to_step};
use crate::ports::OrderRequest;
use crate::runtime::{GridRuntime, GridStatus};

const BINANCE_TAKER_FEE_RATE: f64 = 0.0004;
const REPLACEMENT_SAFETY_BUFFER_BPS: f64 = 5.0;
const BPS_DENOMINATOR: f64 = 10_000.0;

pub struct ReconcileResult {
    pub effects: Vec<ExecutionAction>,
    pub events: Vec<DomainEvent>,
    pub target_exposure: Exposure,
    pub new_status: Option<GridStatus>,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
}

/// 纯函数：给定实例当前状态、价格和风控预算，返回执行计划。
///
/// 这是 engine 的核心协调逻辑：
/// 1. 调用 core::band_status 判断带内/带外
/// 2. 根据状态决定目标占用
/// 3. 调用 core::evaluate_risk 风控拦截
/// 4. 生成 effect 列表（数据，不是 IO）
pub fn reconcile(grid: &GridRuntime, reference_price: f64) -> ReconcileResult {
    let band = strategy::band_status(reference_price, &grid.config);

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

    let decision = risk::evaluate_risk(&intent, &grid.budget);

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
                replacement_gate_reason: None,
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
            replacement_gate_reason: None,
        };
    }

    let delta = grid.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return ReconcileResult {
            effects: vec![ExecutionAction::NoOp],
            events,
            target_exposure: approved_target,
            new_status,
            replacement_gate_reason: None,
        };
    }

    events.push(DomainEvent::ExposureTargetChanged {
        from: grid.current_exposure.clone(),
        to: approved_target.clone(),
    });

    let side = Side::from_exposure(&delta).expect("non-zero delta must have side");
    let rules = &grid.exchange_rules;
    let price = round_to_step(reference_price, rules.price_tick);
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
            replacement_gate_reason: None,
        };
    }

    if let Some(pending_order) = grid.pending_order.as_ref() {
        if let Some(reason) =
            replacement_gate_reason(pending_order, side, price, quantity, reference_price, grid)
        {
            return ReconcileResult {
                effects: vec![ExecutionAction::NoOp],
                events,
                target_exposure: approved_target,
                new_status,
                replacement_gate_reason: Some(reason),
            };
        }
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
        replacement_gate_reason: None,
    }
}

fn resolve_in_band_status(grid: &GridRuntime) -> Option<GridStatus> {
    match grid.status {
        GridStatus::WaitingMarketData => Some(GridStatus::Active),
        GridStatus::Frozen | GridStatus::Holding => Some(GridStatus::Active),
        _ => None,
    }
}

fn replacement_gate_reason(
    pending_order: &crate::runtime::PendingOrder,
    candidate_side: Side,
    candidate_price: f64,
    candidate_quantity: f64,
    reference_price: f64,
    grid: &GridRuntime,
) -> Option<ReplacementGateReason> {
    if candidate_matches_pending_order(
        pending_order,
        candidate_side,
        candidate_price,
        candidate_quantity,
        &grid.exchange_rules,
    ) {
        return Some(ReplacementGateReason::RoundedMatch);
    }

    if pending_order.is_submit_recovery_anchor() {
        return None;
    }

    if pending_order.side != candidate_side {
        return None;
    }

    if !rounded_values_match(
        pending_order.quantity,
        candidate_quantity,
        grid.exchange_rules.quantity_step,
    ) {
        return None;
    }

    let improvement_ratio = replacement_improvement_ratio(
        pending_order,
        candidate_side,
        candidate_price,
        reference_price,
    );
    let threshold_rate = replacement_threshold_rate(grid);
    (improvement_ratio < threshold_rate).then(|| ReplacementGateReason::ImprovementBelowThreshold {
        improvement_bps: ratio_to_bps(improvement_ratio),
        threshold_bps: ratio_to_bps(threshold_rate),
    })
}

fn candidate_matches_pending_order(
    pending_order: &crate::runtime::PendingOrder,
    candidate_side: Side,
    candidate_price: f64,
    candidate_quantity: f64,
    rules: &grid_core::types::ExchangeRules,
) -> bool {
    pending_order.side == candidate_side
        && rounded_values_match(pending_order.price, candidate_price, rules.price_tick)
        && rounded_values_match(
            pending_order.quantity,
            candidate_quantity,
            rules.quantity_step,
        )
}

fn replacement_improvement_ratio(
    pending_order: &crate::runtime::PendingOrder,
    candidate_side: Side,
    candidate_price: f64,
    reference_price: f64,
) -> f64 {
    let price_improvement = match candidate_side {
        Side::Buy => pending_order.price - candidate_price,
        Side::Sell => candidate_price - pending_order.price,
    };

    if price_improvement <= 0.0 {
        return 0.0;
    }

    price_improvement / reference_price.abs().max(f64::EPSILON)
}

fn replacement_threshold_rate(grid: &GridRuntime) -> f64 {
    round_trip_taker_fee_rate(grid) + REPLACEMENT_SAFETY_BUFFER_BPS / BPS_DENOMINATOR
}

fn ratio_to_bps(rate: f64) -> f64 {
    ((rate * BPS_DENOMINATOR) * 10.0).round() / 10.0
}

fn round_trip_taker_fee_rate(grid: &GridRuntime) -> f64 {
    match grid.instrument.venue {
        crate::grid::Venue::Binance => BINANCE_TAKER_FEE_RATE * 2.0,
    }
}

fn rounded_values_match(left: f64, right: f64, step: f64) -> bool {
    let tolerance = if step <= f64::EPSILON {
        f64::EPSILON * 16.0
    } else {
        (step * 1e-9).max(f64::EPSILON * 16.0)
    };
    (left - right).abs() <= tolerance
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
    use grid_core::events::ReplacementGateReason;
    use grid_core::risk::CapacityBudget;
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
            test_budget(),
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

    fn pending_order(
        side: grid_core::types::Side,
        price: f64,
        quantity: f64,
        target: Exposure,
    ) -> crate::runtime::PendingOrder {
        crate::runtime::PendingOrder {
            order_id: Some("order-1".to_string()),
            client_order_id: "client-1".to_string(),
            side,
            price,
            quantity,
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

        let result = reconcile(&grid, 100.0);
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

        let result = reconcile(&grid, 90.0);
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

        let result = reconcile(&grid, 85.0);
        assert_eq!(result.new_status, Some(GridStatus::Frozen));
    }

    #[test]
    fn reconcile_activates_on_first_price() {
        let grid = test_runtime();
        assert_eq!(grid.status, GridStatus::WaitingMarketData);

        let result = reconcile(&grid, 100.0);
        assert_eq!(result.new_status, Some(GridStatus::Active));
    }

    #[test]
    fn reconcile_reactivates_after_reenter() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Frozen;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile(&grid, 100.0);
        assert_eq!(result.new_status, Some(GridStatus::Active));
    }

    #[test]
    fn reconcile_reduce_only_targets_zero() {
        let mut grid = test_runtime();
        grid.config.out_of_band_policy = OutOfBandPolicy::ReduceOnly;
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile(&grid, 85.0);
        assert_eq!(result.new_status, Some(GridStatus::ReducingOnly));
        assert!((result.target_exposure.0).abs() < 0.001);
    }

    #[test]
    fn reconcile_terminate_targets_zero() {
        let mut grid = test_runtime();
        grid.config.out_of_band_policy = OutOfBandPolicy::Terminate;
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile(&grid, 85.0);
        assert_eq!(result.new_status, Some(GridStatus::Terminated));
        assert!((result.target_exposure.0).abs() < 0.001);
    }

    #[test]
    fn reconcile_emits_risk_cap_event_when_budget_caps_target() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile(&grid, 90.0);

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
        grid.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile(&grid, 90.0);

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

        let result = reconcile(&grid, 90.0);

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

        let result = reconcile(&grid, 90.0);

        assert!(matches!(
            result.effects.as_slice(),
            [ExecutionAction::SubmitOrder { .. }]
        ));
    }

    #[test]
    fn reconcile_does_not_resubmit_when_pending_order_already_matches_candidate_order() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.pending_order = Some(pending_order(
            grid_core::types::Side::Buy,
            90.0,
            30.0,
            Exposure(8.0),
        ));

        let result = reconcile(&grid, 90.0);
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
    }

    #[test]
    fn reconcile_keeps_existing_pending_order_when_candidate_order_matches_exchange_rounded_values()
    {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(2.0);
        grid.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.5,
            min_qty: 0.0,
            min_notional: 0.0,
        };
        grid.pending_order = Some(pending_order(
            grid_core::types::Side::Sell,
            99.9,
            7.0,
            Exposure(0.5),
        ));

        let result = reconcile(&grid, 99.95);

        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
        assert_eq!(
            result.replacement_gate_reason,
            Some(ReplacementGateReason::RoundedMatch)
        );
    }

    #[test]
    fn reconcile_replaces_pending_order_when_same_side_quantity_differs_even_if_price_is_worse() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(2.0);
        grid.pending_order = Some(pending_order(
            grid_core::types::Side::Sell,
            100.0,
            1.0,
            Exposure(1.7),
        ));

        let result = reconcile(&grid, 99.0);

        assert!(matches!(
            result.effects.as_slice(),
            [
                ExecutionAction::CancelAll { .. },
                ExecutionAction::SubmitOrder { .. }
            ]
        ));
        assert_eq!(result.replacement_gate_reason, None);
    }

    #[test]
    fn reconcile_keeps_existing_pending_order_when_price_improvement_does_not_cover_replacement_threshold()
     {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.pending_order = Some(pending_order(
            grid_core::types::Side::Buy,
            100.0,
            0.1,
            Exposure(0.4),
        ));

        let result = reconcile(&grid, 99.95);

        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
        assert_eq!(
            result.replacement_gate_reason,
            Some(ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps: 10.0,
                threshold_bps: 13.0,
            })
        );
    }

    #[test]
    fn reconcile_replaces_pending_order_when_price_improvement_covers_replacement_threshold() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.pending_order = Some(pending_order(
            grid_core::types::Side::Buy,
            100.0,
            0.3,
            Exposure(0.4),
        ));

        let result = reconcile(&grid, 99.89);

        assert!(matches!(
            result.effects.as_slice(),
            [
                ExecutionAction::CancelAll { .. },
                ExecutionAction::SubmitOrder { .. }
            ]
        ));
        assert_eq!(result.replacement_gate_reason, None);
    }

    #[test]
    fn reconcile_replaces_pending_order_when_side_flips() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.pending_order = Some(pending_order(
            grid_core::types::Side::Buy,
            99.9,
            0.1,
            Exposure(0.1),
        ));

        let result = reconcile(&grid, 100.2);

        assert!(matches!(
            result.effects.as_slice(),
            [
                ExecutionAction::CancelAll { .. },
                ExecutionAction::SubmitOrder { .. }
            ]
        ));
    }

    #[test]
    fn freeze_keeps_last_in_band_target_instead_of_current_exposure() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(4.0);
        grid.target_exposure = Some(Exposure(6.0));
        grid.config.out_of_band_policy = OutOfBandPolicy::Freeze;

        let result = reconcile(&grid, 85.0);

        assert_eq!(result.target_exposure.0, 6.0);
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
    }

    #[test]
    fn reconcile_replaces_pending_order_when_target_differs_and_quantity_changes() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.pending_order = Some(test_pending_order(Exposure(4.0)));

        let result = reconcile(&grid, 90.0);

        assert!(matches!(
            result.effects.as_slice(),
            [
                ExecutionAction::CancelAll { .. },
                ExecutionAction::SubmitOrder { .. }
            ]
        ));
    }

    #[test]
    fn reconcile_replans_when_submit_recovery_anchor_target_differs() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.pending_order = Some(crate::runtime::PendingOrder {
            order_id: None,
            client_order_id: "recover-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.0,
            quantity: 0.25,
            target_exposure: Exposure(6.0),
            status: crate::ports::OrderStatus::Submitting,
        });

        let result = reconcile(&grid, 95.0);

        assert_eq!(result.target_exposure, Exposure(4.0));
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

        let result = reconcile(&grid, 99.09);

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

        let result = reconcile(&grid, 99.09);
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| !matches!(effect, ExecutionAction::NoOp))
        );
    }

    #[test]
    fn reconcile_uses_budget_from_runtime() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile(&grid, 90.0);

        assert_eq!(result.target_exposure, Exposure(4.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied {
                intended,
                capped,
            } if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
    }
}
