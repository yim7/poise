use grid_core::events::DomainEvent;
use grid_core::risk::{self, ExposureIntent, RiskDecision};
use grid_core::strategy::{self, BandStatus, OutOfBandPolicy};
use grid_core::types::Exposure;

use crate::runtime::{GridRuntime, GridStatus};

pub struct TargetReconcileResult {
    pub events: Vec<DomainEvent>,
    pub target_exposure: Exposure,
    pub new_status: Option<GridStatus>,
    pub suppress_execution: bool,
}

pub fn reconcile_target(grid: &GridRuntime, reference_price: f64) -> TargetReconcileResult {
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
        RiskDecision::Allow(target) => (target, vec![]),
        RiskDecision::Cap(capped) => (
            capped.clone(),
            vec![DomainEvent::RiskCapApplied {
                intended: target.clone(),
                capped,
            }],
        ),
        RiskDecision::Deny { reason } => {
            return TargetReconcileResult {
                events: vec![DomainEvent::RiskDenied { reason }],
                target_exposure: grid.current_exposure.clone(),
                new_status: None,
                suppress_execution: true,
            };
        }
    };

    let would_increase_risk_out_of_band = matches!(
        band,
        BandStatus::OutOfBand {
            policy: OutOfBandPolicy::Freeze | OutOfBandPolicy::Hold,
            ..
        }
    ) && approved_target.0.abs()
        > grid.current_exposure.0.abs();

    if would_increase_risk_out_of_band {
        return TargetReconcileResult {
            events,
            target_exposure: approved_target,
            new_status,
            suppress_execution: true,
        };
    }

    let delta = grid.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return TargetReconcileResult {
            events,
            target_exposure: approved_target,
            new_status,
            suppress_execution: true,
        };
    }

    events.push(DomainEvent::ExposureTargetChanged {
        from: grid.current_exposure.clone(),
        to: approved_target.clone(),
    });

    TargetReconcileResult {
        events,
        target_exposure: approved_target,
        new_status,
        suppress_execution: false,
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

    fn test_budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 4.0,
        }
    }

    #[test]
    fn reconcile_target_suppresses_execution_when_exposure_unchanged() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);

        let result = reconcile_target(&grid, 100.0);

        assert!(result.suppress_execution);
        assert_eq!(result.target_exposure, Exposure(0.0));
    }

    #[test]
    fn reconcile_target_emits_event_when_exposure_changes() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);

        let result = reconcile_target(&grid, 90.0);

        assert!((result.target_exposure.0 - 8.0).abs() < 0.001);
        assert!(!result.suppress_execution);
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ExposureTargetChanged { .. }))
        );
    }

    #[test]
    fn reconcile_target_freezes_when_out_of_band() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile_target(&grid, 85.0);

        assert_eq!(result.new_status, Some(GridStatus::Frozen));
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_activates_on_first_price() {
        let grid = test_runtime();

        let result = reconcile_target(&grid, 100.0);

        assert_eq!(result.new_status, Some(GridStatus::Active));
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_reactivates_after_reenter() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Frozen;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile_target(&grid, 100.0);

        assert_eq!(result.new_status, Some(GridStatus::Active));
        assert!(!result.suppress_execution);
    }

    #[test]
    fn reconcile_target_reduce_only_targets_zero() {
        let mut grid = test_runtime();
        grid.config.out_of_band_policy = OutOfBandPolicy::ReduceOnly;
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile_target(&grid, 85.0);

        assert_eq!(result.new_status, Some(GridStatus::ReducingOnly));
        assert!(result.target_exposure.0.abs() < 0.001);
    }

    #[test]
    fn reconcile_target_terminate_targets_zero() {
        let mut grid = test_runtime();
        grid.config.out_of_band_policy = OutOfBandPolicy::Terminate;
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(8.0);

        let result = reconcile_target(&grid, 85.0);

        assert_eq!(result.new_status, Some(GridStatus::Terminated));
        assert!(result.target_exposure.0.abs() < 0.001);
    }

    #[test]
    fn reconcile_target_emits_risk_cap_event_when_budget_caps_target() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile_target(&grid, 90.0);

        assert_eq!(result.target_exposure, Exposure(4.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied { intended, capped }
                if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
    }

    #[test]
    fn reconcile_target_keeps_risk_cap_event_when_cap_matches_current_exposure() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(4.0);
        grid.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile_target(&grid, 90.0);

        assert!(result.suppress_execution);
        assert_eq!(result.target_exposure, Exposure(4.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied { intended, capped }
                if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ExposureTargetChanged { .. }))
        );
    }

    #[test]
    fn reconcile_target_uses_risk_state_pnl_to_cap_target() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(2.0);
        grid.risk_state.realized_pnl_today = -100.0;
        grid.risk_state.unrealized_pnl = -25.0;

        let result = reconcile_target(&grid, 90.0);

        assert_eq!(result.target_exposure, Exposure(0.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied { intended, capped }
                if *intended == Exposure(8.0) && *capped == Exposure(0.0)
        )));
    }

    #[test]
    fn freeze_keeps_last_in_band_target_instead_of_current_exposure() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(4.0);
        grid.target_exposure = Some(Exposure(6.0));
        grid.config.out_of_band_policy = OutOfBandPolicy::Freeze;

        let result = reconcile_target(&grid, 85.0);

        assert_eq!(result.target_exposure.0, 6.0);
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_uses_budget_from_runtime() {
        let mut grid = test_runtime();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(0.0);
        grid.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile_target(&grid, 90.0);

        assert_eq!(result.target_exposure, Exposure(4.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied { intended, capped }
                if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
    }
}
