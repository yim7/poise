use poise_core::events::DomainEvent;
use poise_core::risk::{self, ExposureIntent, RiskDecision};
use poise_core::strategy::{self, BandStatus, OutOfBandPolicy};
use poise_core::types::Exposure;

use crate::runtime::{AccountCapacityConstraint, TrackRuntime, TrackStatus};

pub struct TargetReconcileResult {
    pub events: Vec<DomainEvent>,
    pub desired_exposure: Exposure,
    pub new_status: Option<TrackStatus>,
    pub suppress_execution: bool,
}

pub fn reconcile_target(track: &TrackRuntime, reference_price: f64) -> TargetReconcileResult {
    if matches!(track.status, TrackStatus::Terminated) {
        let target = Exposure(0.0);
        let delta = track.current_exposure.delta(&target);
        return TargetReconcileResult {
            events: (!delta.is_zero())
                .then_some(DomainEvent::ExposureTargetChanged {
                    from: track.current_exposure.clone(),
                    to: target.clone(),
                })
                .into_iter()
                .collect(),
            desired_exposure: target,
            new_status: Some(TrackStatus::Terminated),
            suppress_execution: delta.is_zero(),
        };
    }

    if let Some(target_override) = track.manual_target_override.clone() {
        let delta = track.current_exposure.delta(&target_override);
        return TargetReconcileResult {
            events: (!delta.is_zero())
                .then_some(DomainEvent::ExposureTargetChanged {
                    from: track.current_exposure.clone(),
                    to: target_override.clone(),
                })
                .into_iter()
                .collect(),
            desired_exposure: target_override,
            new_status: Some(TrackStatus::ReducingOnly),
            suppress_execution: delta.is_zero(),
        };
    }

    let band = strategy::band_status(reference_price, &track.config);

    let (target, new_status) = match &band {
        BandStatus::InBand { target } => (target.clone(), resolve_in_band_status(track)),
        BandStatus::OutOfBand { policy, .. } => apply_out_of_band(track, *policy),
    };

    let intent = ExposureIntent {
        current: track.current_exposure.clone(),
        target: target.clone(),
        unit_notional: track.config.notional_per_unit,
        realized_pnl_today: track.risk_state.realized_pnl_today,
        unrealized_pnl: track.risk_state.unrealized_pnl,
    };

    let decision = risk::evaluate_risk(&intent, &track.budget);

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
                desired_exposure: track.current_exposure.clone(),
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
        > track.current_exposure.0.abs();

    if would_increase_risk_out_of_band {
        return TargetReconcileResult {
            events,
            desired_exposure: approved_target,
            new_status,
            suppress_execution: true,
        };
    }

    if let Some(reason) = account_capacity_denial_reason(
        &track.current_exposure,
        &approved_target,
        track.config.notional_per_unit,
        &track.risk_state.account_capacity_constraint,
    ) {
        return TargetReconcileResult {
            events: vec![DomainEvent::RiskDenied { reason }],
            desired_exposure: track.current_exposure.clone(),
            new_status: None,
            suppress_execution: true,
        };
    }

    let delta = track.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return TargetReconcileResult {
            events,
            desired_exposure: approved_target,
            new_status,
            suppress_execution: true,
        };
    }

    events.push(DomainEvent::ExposureTargetChanged {
        from: track.current_exposure.clone(),
        to: approved_target.clone(),
    });

    TargetReconcileResult {
        events,
        desired_exposure: approved_target,
        new_status,
        suppress_execution: false,
    }
}

fn resolve_in_band_status(track: &TrackRuntime) -> Option<TrackStatus> {
    match track.status {
        TrackStatus::WaitingMarketData => Some(TrackStatus::Active),
        TrackStatus::Frozen | TrackStatus::Holding => Some(TrackStatus::Active),
        _ => None,
    }
}

fn apply_out_of_band(
    track: &TrackRuntime,
    policy: OutOfBandPolicy,
) -> (Exposure, Option<TrackStatus>) {
    let frozen_target = track
        .desired_exposure
        .clone()
        .unwrap_or_else(|| track.current_exposure.clone());

    match policy {
        OutOfBandPolicy::Freeze => (frozen_target, Some(TrackStatus::Frozen)),
        OutOfBandPolicy::Hold => (frozen_target, Some(TrackStatus::Holding)),
        OutOfBandPolicy::ReduceOnly => (Exposure(0.0), Some(TrackStatus::ReducingOnly)),
        OutOfBandPolicy::Terminate => (Exposure(0.0), Some(TrackStatus::Terminated)),
    }
}

fn account_capacity_denial_reason(
    current: &Exposure,
    target: &Exposure,
    unit_notional: f64,
    constraint: &AccountCapacityConstraint,
) -> Option<String> {
    let required_increase_notional =
        additional_increase_notional_required(current, target, unit_notional);
    if required_increase_notional <= f64::EPSILON {
        return None;
    }

    if constraint.increase_blocked {
        return Some(
            constraint
                .blocked_reason
                .clone()
                .unwrap_or_else(|| "insufficient account margin".to_string()),
        );
    }

    if let Some(max_increase_notional) = constraint.max_increase_notional
        && required_increase_notional > max_increase_notional + f64::EPSILON
    {
        return Some("insufficient account margin".to_string());
    }

    None
}

fn additional_increase_notional_required(
    current: &Exposure,
    target: &Exposure,
    unit_notional: f64,
) -> f64 {
    if !unit_notional.is_finite() || unit_notional <= 0.0 {
        return 0.0;
    }

    let current_abs = current.0.abs();
    let target_abs = target.0.abs();
    if target_abs <= current_abs {
        return 0.0;
    }

    (target_abs - current_abs) * unit_notional
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::AccountCapacityConstraint;
    use chrono::{TimeZone, Utc};
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::*;

    fn test_runtime() -> TrackRuntime {
        TrackRuntime::new(
            "test".into(),
            crate::track::Instrument::new(crate::track::Venue::Binance, "BTCUSDT"),
            TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            test_budget(),
            poise_core::types::ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.1,
                min_qty: 0.0,
                min_notional: 0.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
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
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(0.0);

        let result = reconcile_target(&track, 100.0);

        assert!(result.suppress_execution);
        assert_eq!(result.desired_exposure, Exposure(0.0));
    }

    #[test]
    fn reconcile_target_emits_event_when_exposure_changes() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(0.0);

        let result = reconcile_target(&track, 90.0);

        assert!((result.desired_exposure.0 - 8.0).abs() < 0.001);
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
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.new_status, Some(TrackStatus::Frozen));
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_activates_on_first_price() {
        let track = test_runtime();

        let result = reconcile_target(&track, 100.0);

        assert_eq!(result.new_status, Some(TrackStatus::Active));
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_reactivates_after_reenter() {
        let mut track = test_runtime();
        track.status = TrackStatus::Frozen;
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 100.0);

        assert_eq!(result.new_status, Some(TrackStatus::Active));
        assert!(!result.suppress_execution);
    }

    #[test]
    fn reconcile_target_reduce_only_targets_zero() {
        let mut track = test_runtime();
        track.config.out_of_band_policy = OutOfBandPolicy::ReduceOnly;
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.new_status, Some(TrackStatus::ReducingOnly));
        assert!(result.desired_exposure.0.abs() < 0.001);
    }

    #[test]
    fn reconcile_target_terminate_targets_zero() {
        let mut track = test_runtime();
        track.config.out_of_band_policy = OutOfBandPolicy::Terminate;
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.new_status, Some(TrackStatus::Terminated));
        assert!(result.desired_exposure.0.abs() < 0.001);
    }

    #[test]
    fn reconcile_target_emits_risk_cap_event_when_budget_caps_target() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(0.0);
        track.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.desired_exposure, Exposure(4.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied { intended, capped }
                if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
    }

    #[test]
    fn reconcile_target_keeps_risk_cap_event_when_cap_matches_current_exposure() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(4.0);
        track.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile_target(&track, 90.0);

        assert!(result.suppress_execution);
        assert_eq!(result.desired_exposure, Exposure(4.0));
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
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(2.0);
        track.risk_state.realized_pnl_today = -100.0;
        track.risk_state.unrealized_pnl = -25.0;

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.desired_exposure, Exposure(0.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied { intended, capped }
                if *intended == Exposure(8.0) && *capped == Exposure(0.0)
        )));
    }

    #[test]
    fn freeze_keeps_last_in_band_target_instead_of_current_exposure() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(4.0);
        track.desired_exposure = Some(Exposure(6.0));
        track.config.out_of_band_policy = OutOfBandPolicy::Freeze;

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.desired_exposure.0, 6.0);
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_uses_budget_from_runtime() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(0.0);
        track.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.desired_exposure, Exposure(4.0));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::RiskCapApplied { intended, capped }
                if *intended == Exposure(8.0) && *capped == Exposure(4.0)
        )));
    }

    #[test]
    fn margin_guard_reconcile_denies_risk_increase_when_guard_is_active() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(1.0);
        track.risk_state.account_capacity_constraint = AccountCapacityConstraint {
            increase_blocked: true,
            blocked_reason: Some("insufficient_margin".into()),
            max_increase_notional: Some(1_000.0),
        };

        let result = reconcile_target(&track, 90.0);

        assert!(result.suppress_execution);
        assert_eq!(result.desired_exposure, track.current_exposure);
        assert_eq!(
            result.events,
            vec![DomainEvent::RiskDenied {
                reason: "insufficient_margin".into()
            }]
        );
    }

    #[test]
    fn margin_guard_reconcile_denies_when_required_notional_exceeds_available_capacity() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(1.0);
        track.risk_state.account_capacity_constraint = AccountCapacityConstraint {
            increase_blocked: false,
            blocked_reason: None,
            max_increase_notional: Some(500.0),
        };

        let result = reconcile_target(&track, 90.0);

        assert!(result.suppress_execution);
        assert_eq!(result.desired_exposure, track.current_exposure);
        assert_eq!(
            result.events,
            vec![DomainEvent::RiskDenied {
                reason: "insufficient account margin".into()
            }]
        );
    }

    #[test]
    fn margin_guard_reconcile_allows_reduce_only_target() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(5.0);
        track.manual_target_override = Some(Exposure(2.0));
        track.risk_state.account_capacity_constraint = AccountCapacityConstraint {
            increase_blocked: true,
            blocked_reason: Some("insufficient_margin".into()),
            max_increase_notional: Some(0.0),
        };

        let result = reconcile_target(&track, 100.0);

        assert!(!result.suppress_execution);
        assert_eq!(result.desired_exposure, Exposure(2.0));
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ExposureTargetChanged { .. }))
        );
    }
}
