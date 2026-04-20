use poise_core::events::DomainEvent;
use poise_core::risk::{self, ExposureIntent, RiskDecision};
use poise_core::strategy::{self, BandProtectionPolicy, BandStatus};
use poise_core::types::Exposure;

use crate::loss_guard::build_loss_guard_snapshot;
use crate::runtime::{AccountCapacityConstraint, AppliedRiskCap, TrackRuntime, TrackStatus};

pub struct TargetReconcileResult {
    pub events: Vec<DomainEvent>,
    pub desired_exposure: Exposure,
    pub applied_risk_cap: Option<AppliedRiskCap>,
    pub new_status: Option<TrackStatus>,
    pub suppress_execution: bool,
}

pub fn reconcile_target(track: &TrackRuntime, strategy_price: f64) -> TargetReconcileResult {
    if matches!(track.status, TrackStatus::Terminated) {
        let target = Exposure(0.0);
        let delta = track.current_exposure.delta(&target);
        return TargetReconcileResult {
            events: exposure_target_change_event(track, &target)
                .into_iter()
                .collect(),
            desired_exposure: target,
            applied_risk_cap: None,
            new_status: Some(TrackStatus::Terminated),
            suppress_execution: delta.is_zero(),
        };
    }

    if let Some(target_override) = track.manual_target_override.clone() {
        let delta = track.current_exposure.delta(&target_override);
        return TargetReconcileResult {
            events: exposure_target_change_event(track, &target_override)
                .into_iter()
                .collect(),
            desired_exposure: target_override,
            applied_risk_cap: None,
            new_status: Some(TrackStatus::ManualFlattening),
            suppress_execution: delta.is_zero(),
        };
    }

    let band = strategy::band_status(strategy_price, &track.config);

    let (target, new_status) = match &band {
        BandStatus::InBand { target } => (
            resolve_in_band_target(track, target),
            resolve_in_band_status(track),
        ),
        BandStatus::OutOfBand { policy, .. } => apply_out_of_band(track, *policy),
    };

    let intent = ExposureIntent {
        current: track.current_exposure.clone(),
        target: target.clone(),
        unit_notional: track.config.notional_per_unit,
        loss_guard: build_loss_guard_snapshot(&track.ledger_state, &track.risk_state),
    };

    let decision = risk::evaluate_risk(&intent, &track.budget);

    let (approved_target, applied_risk_cap, mut events) = match decision {
        RiskDecision::Allow(target) => (target, None, vec![]),
        RiskDecision::Cap(capped) => {
            let applied_risk_cap = AppliedRiskCap {
                intended: target.clone(),
                capped: capped.clone(),
            };
            let event = risk_cap_applied_event(track, &applied_risk_cap);
            (capped, Some(applied_risk_cap), event.into_iter().collect())
        }
        RiskDecision::Deny { reason } => {
            return TargetReconcileResult {
                events: vec![DomainEvent::RiskDenied { reason }],
                desired_exposure: track.current_exposure.clone(),
                applied_risk_cap: None,
                new_status: None,
                suppress_execution: true,
            };
        }
    };

    let would_increase_risk_out_of_band = matches!(
        band,
        BandStatus::OutOfBand {
            policy: BandProtectionPolicy::Freeze { .. } | BandProtectionPolicy::Hold,
            ..
        }
    ) && approved_target.0.abs()
        > track.current_exposure.0.abs();

    if would_increase_risk_out_of_band {
        return TargetReconcileResult {
            events,
            desired_exposure: approved_target,
            applied_risk_cap,
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
            applied_risk_cap: None,
            new_status: None,
            suppress_execution: true,
        };
    }

    let delta = track.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return TargetReconcileResult {
            events,
            desired_exposure: approved_target,
            applied_risk_cap,
            new_status,
            suppress_execution: true,
        };
    }

    if let Some(event) = exposure_target_change_event(track, &approved_target) {
        events.push(event);
    }

    TargetReconcileResult {
        events,
        desired_exposure: approved_target,
        applied_risk_cap,
        new_status,
        suppress_execution: false,
    }
}

fn exposure_target_change_event(
    track: &TrackRuntime,
    next_target: &Exposure,
) -> Option<DomainEvent> {
    let previous_target = track
        .desired_exposure
        .clone()
        .unwrap_or_else(|| track.current_exposure.clone());
    (previous_target != *next_target).then_some(DomainEvent::ExposureTargetChanged {
        from: previous_target,
        to: next_target.clone(),
    })
}

fn risk_cap_applied_event(
    track: &TrackRuntime,
    applied_risk_cap: &AppliedRiskCap,
) -> Option<DomainEvent> {
    (track.active_risk_cap.as_ref() != Some(applied_risk_cap)).then_some(
        DomainEvent::RiskCapApplied {
            intended: applied_risk_cap.intended.clone(),
            capped: applied_risk_cap.capped.clone(),
        },
    )
}

fn resolve_in_band_status(track: &TrackRuntime) -> Option<TrackStatus> {
    match track.status {
        TrackStatus::WaitingMarketData | TrackStatus::Flattening => Some(TrackStatus::Active),
        TrackStatus::Frozen => Some(TrackStatus::Active),
        TrackStatus::Holding => None,
        _ => None,
    }
}

fn resolve_in_band_target(track: &TrackRuntime, target: &Exposure) -> Exposure {
    if matches!(track.status, TrackStatus::Holding) {
        track
            .desired_exposure
            .clone()
            .unwrap_or_else(|| track.current_exposure.clone())
    } else {
        target.clone()
    }
}

fn apply_out_of_band(
    track: &TrackRuntime,
    policy: BandProtectionPolicy,
) -> (Exposure, Option<TrackStatus>) {
    let frozen_target = track
        .desired_exposure
        .clone()
        .unwrap_or_else(|| track.current_exposure.clone());

    match policy {
        BandProtectionPolicy::Freeze { .. } => (frozen_target, Some(TrackStatus::Frozen)),
        BandProtectionPolicy::Hold => (frozen_target, Some(TrackStatus::Holding)),
        BandProtectionPolicy::Flatten { .. } => (Exposure(0.0), Some(TrackStatus::Flattening)),
        BandProtectionPolicy::Terminate => (Exposure(0.0), Some(TrackStatus::Terminated)),
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
                out_of_band_policy: BandProtectionPolicy::Freeze {
                    recover: BandRecoverPolicy::BackInBand,
                },
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
            daily_loss_limit: 120.0,
            total_loss_limit: 500.0,
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
    fn reconcile_target_does_not_repeat_event_when_desired_exposure_is_unchanged() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(0.0);
        track.desired_exposure = Some(Exposure(8.0));

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.desired_exposure, Exposure(8.0));
        assert!(!result.suppress_execution);
        assert!(
            !result
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
    fn frozen_recovers_to_active_when_price_returns_in_band() {
        let mut track = test_runtime();
        track.status = TrackStatus::Frozen;
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 100.0);

        assert_eq!(result.new_status, Some(TrackStatus::Active));
    }

    #[test]
    fn holding_stays_holding_when_price_returns_in_band() {
        let mut track = test_runtime();
        track.status = TrackStatus::Holding;
        track.current_exposure = Exposure(8.0);
        track.desired_exposure = Some(Exposure(3.0));

        let result = reconcile_target(&track, 100.0);

        assert_eq!(result.new_status, None);
        assert_eq!(result.desired_exposure, Exposure(3.0));
    }

    #[test]
    fn reconcile_target_flatten_policy_enters_flattening() {
        let mut track = test_runtime();
        track.config.out_of_band_policy = serde_json::from_str("\"flatten\"").unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.desired_exposure, Exposure(0.0));
        assert_eq!(
            serde_json::to_string(&result.new_status.unwrap()).unwrap(),
            "\"flattening\""
        );
    }

    #[test]
    fn reconcile_target_flatten_targets_zero() {
        let mut track = test_runtime();
        track.config.out_of_band_policy = BandProtectionPolicy::Flatten {
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
        };
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.new_status, Some(TrackStatus::Flattening));
        assert!(result.desired_exposure.0.abs() < 0.001);
    }

    #[test]
    fn reconcile_target_terminate_targets_zero() {
        let mut track = test_runtime();
        track.config.out_of_band_policy = BandProtectionPolicy::Terminate;
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
    fn reconcile_target_emits_risk_cap_event_when_cap_is_new_even_if_capped_target_matches_desired()
    {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(0.0);
        track.desired_exposure = Some(Exposure(4.0));
        track.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.desired_exposure, Exposure(4.0));
        assert!(!result.suppress_execution);
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
        assert_eq!(
            result.applied_risk_cap,
            Some(AppliedRiskCap {
                intended: Exposure(8.0),
                capped: Exposure(4.0),
            })
        );
    }

    #[test]
    fn reconcile_target_does_not_repeat_risk_cap_event_when_same_cap_is_already_active() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(0.0);
        track.desired_exposure = Some(Exposure(4.0));
        track.active_risk_cap = Some(AppliedRiskCap {
            intended: Exposure(8.0),
            capped: Exposure(4.0),
        });
        track.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.desired_exposure, Exposure(4.0));
        assert!(!result.suppress_execution);
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::RiskCapApplied { .. }))
        );
        assert_eq!(result.applied_risk_cap, track.active_risk_cap);
    }

    #[test]
    fn reconcile_builds_loss_guard_snapshot_from_ledger_accessors() {
        let mut track = test_runtime();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(2.0);
        track.ledger_state.realized_pnl_day =
            Some(chrono::NaiveDate::from_ymd_opt(2026, 4, 8).unwrap());
        track.ledger_state.gross_realized_pnl_today = 100.0;
        track.ledger_state.gross_realized_pnl_cumulative = 320.0;
        track.ledger_state.trading_fee_today = 8.0;
        track.ledger_state.trading_fee_cumulative = 20.0;
        track.ledger_state.funding_fee_today = -2.0;
        track.ledger_state.funding_fee_cumulative = -5.0;
        track.risk_state.unrealized_pnl = -215.0;
        track.budget.daily_loss_limit = 120.0;
        track.budget.total_loss_limit = 500.0;

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
        track.config.out_of_band_policy = BandProtectionPolicy::Freeze {
            recover: BandRecoverPolicy::BackInBand,
        };

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
