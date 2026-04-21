use poise_core::events::DomainEvent;
use poise_core::risk::{self, ExposureIntent, RiskOutcome};
use poise_core::strategy::{self, BandBoundary, BandProtectionPolicy, BandStatus};
use poise_core::types::Exposure;

use crate::execution_gate::{AccountCapacityGate, AccountCapacityGateInput, ExecutionGateDecision};
use crate::loss_guard::build_loss_guard_snapshot;
use crate::runtime::{
    AppliedRiskCap, AutoState, BandTerminationCause, ControlState, ManualState, TerminationCause,
    TrackRuntime, TrackState,
};

pub struct TargetReconcileResult {
    pub events: Vec<DomainEvent>,
    pub desired_exposure: Exposure,
    pub applied_risk_cap: Option<AppliedRiskCap>,
    pub new_runtime_state: Option<TrackState>,
    pub execution_gate_decision: ExecutionGateDecision,
    pub suppress_execution: bool,
}

pub fn reconcile_target(track: &TrackRuntime, strategy_price: f64) -> TargetReconcileResult {
    if let TrackState::Terminated { .. } = &track.track_state {
        let target = Exposure(0.0);
        let delta = track.current_exposure.delta(&target);
        return TargetReconcileResult {
            events: exposure_target_change_event(track, &target)
                .into_iter()
                .collect(),
            desired_exposure: target,
            applied_risk_cap: None,
            new_runtime_state: Some(track.track_state.clone()),
            execution_gate_decision: ExecutionGateDecision::Open,
            suppress_execution: delta.is_zero(),
        };
    }

    if let TrackState::Running(ControlState::Manual(manual_state)) = &track.track_state {
        let target_override = match manual_state {
            ManualState::Flattened => Exposure(0.0),
            ManualState::TargetOverride { target } => target.clone(),
        };
        let delta = track.current_exposure.delta(&target_override);
        return TargetReconcileResult {
            events: exposure_target_change_event(track, &target_override)
                .into_iter()
                .collect(),
            desired_exposure: target_override,
            applied_risk_cap: None,
            new_runtime_state: Some(track.track_state.clone()),
            execution_gate_decision: ExecutionGateDecision::Open,
            suppress_execution: delta.is_zero(),
        };
    }

    let band = strategy::band_status(strategy_price, &track.config);

    let (target, mut new_runtime_state) = match &band {
        BandStatus::InBand { target } => (
            resolve_in_band_target(track, target),
            resolve_in_band_state(track, strategy_price),
        ),
        BandStatus::OutOfBand { policy, boundary } => {
            apply_out_of_band(track, strategy_price, *policy, *boundary)
        }
    };

    let intent = ExposureIntent {
        current: track.current_exposure.clone(),
        target: target.clone(),
        unit_notional: track.config.notional_per_unit,
        loss_guard: build_loss_guard_snapshot(&track.ledger_state, &track.risk_state),
    };

    let decision = risk::evaluate_risk_outcome(&intent, &track.budget);

    let (approved_target, applied_risk_cap, mut events) = match decision {
        RiskOutcome::Allow { target } => (target, None, vec![]),
        RiskOutcome::Cap { target: capped } => {
            let applied_risk_cap = AppliedRiskCap {
                intended: target.clone(),
                capped: capped.clone(),
            };
            let event = risk_cap_applied_event(track, &applied_risk_cap);
            (capped, Some(applied_risk_cap), event.into_iter().collect())
        }
        RiskOutcome::Terminate(cause) => {
            let target = Exposure(0.0);
            let delta = track.current_exposure.delta(&target);
            return TargetReconcileResult {
                events: exposure_target_change_event(track, &target)
                    .into_iter()
                    .collect(),
                desired_exposure: target,
                applied_risk_cap: None,
                new_runtime_state: Some(TrackState::Terminated {
                    cause: TerminationCause::Risk(cause),
                }),
                execution_gate_decision: ExecutionGateDecision::Open,
                suppress_execution: delta.is_zero(),
            };
        }
    };

    if let Some(runtime_state) = new_runtime_state.as_mut() {
        match runtime_state {
            TrackState::Running(ControlState::Automatic(AutoState::Frozen {
                target_anchor,
                ..
            }))
            | TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
                target_anchor,
                ..
            })) => {
                *target_anchor = approved_target.clone();
            }
            _ => {}
        }
    }

    if should_suppress_protected_risk_increase(track, &new_runtime_state, &approved_target) {
        return TargetReconcileResult {
            events,
            desired_exposure: approved_target,
            applied_risk_cap,
            new_runtime_state,
            execution_gate_decision: ExecutionGateDecision::Open,
            suppress_execution: true,
        };
    }

    let execution_gate_decision = AccountCapacityGate::evaluate(AccountCapacityGateInput {
        current: track.current_exposure.clone(),
        approved_target: approved_target.clone(),
        unit_notional: track.config.notional_per_unit,
        available_notional: track
            .execution_gate_state
            .account_capacity
            .available_notional,
    });

    if let ExecutionGateDecision::NoSubmit { reason } = &execution_gate_decision {
        if track.execution_gate_state.last_decision != execution_gate_decision {
            events.push(DomainEvent::ExecutionGateApplied {
                reason: reason.clone(),
            });
        }
        return TargetReconcileResult {
            events,
            desired_exposure: approved_target,
            applied_risk_cap,
            new_runtime_state,
            execution_gate_decision,
            suppress_execution: true,
        };
    }

    let delta = track.current_exposure.delta(&approved_target);
    if delta.is_zero() {
        return TargetReconcileResult {
            events,
            desired_exposure: approved_target,
            applied_risk_cap,
            new_runtime_state,
            execution_gate_decision,
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
        new_runtime_state,
        execution_gate_decision,
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

fn resolve_in_band_target(track: &TrackRuntime, target: &Exposure) -> Exposure {
    match &track.track_state {
        TrackState::Running(ControlState::Automatic(AutoState::Frozen {
            target_anchor, ..
        }))
        | TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
            target_anchor,
            ..
        })) => target_anchor.clone(),
        TrackState::Running(ControlState::Automatic(AutoState::Flattening { .. })) => Exposure(0.0),
        _ => target.clone(),
    }
}

fn resolve_in_band_state(track: &TrackRuntime, strategy_price: f64) -> Option<TrackState> {
    match &track.track_state {
        TrackState::WaitingMarketData => Some(TrackState::Running(ControlState::Automatic(
            AutoState::FollowingBand,
        ))),
        TrackState::Running(ControlState::Automatic(AutoState::Frozen { .. }))
        | TrackState::Running(ControlState::Automatic(AutoState::FlattenPending { .. })) => Some(
            TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)),
        ),
        TrackState::Running(ControlState::Automatic(AutoState::Flattening { boundary })) => {
            let BandProtectionPolicy::Flatten { recover, .. } = track.config.out_of_band_policy
            else {
                return Some(TrackState::Running(ControlState::Automatic(
                    AutoState::FollowingBand,
                )));
            };
            strategy::band_reentry_price_confirmed(
                strategy_price,
                &recover,
                track.config.lower_price,
                track.config.upper_price,
                *boundary,
            )
            .then_some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            )))
        }
        _ => None,
    }
}

fn apply_out_of_band(
    track: &TrackRuntime,
    strategy_price: f64,
    policy: BandProtectionPolicy,
    boundary: BandBoundary,
) -> (Exposure, Option<TrackState>) {
    match policy {
        BandProtectionPolicy::Freeze => freeze_with_target_anchor(track),
        BandProtectionPolicy::Flatten { trigger_bps, .. } => {
            flatten_with_trigger_guard(track, strategy_price, boundary, trigger_bps)
        }
        BandProtectionPolicy::Terminate => (
            Exposure(0.0),
            Some(TrackState::Terminated {
                cause: TerminationCause::Band(BandTerminationCause::OutOfRange),
            }),
        ),
    }
}

fn freeze_with_target_anchor(track: &TrackRuntime) -> (Exposure, Option<TrackState>) {
    let target_anchor = protection_target_anchor(track);
    if matches!(
        track.track_state,
        TrackState::Running(ControlState::Automatic(AutoState::Frozen { .. }))
    ) {
        return (target_anchor, None);
    }
    (
        target_anchor.clone(),
        Some(TrackState::Running(ControlState::Automatic(
            AutoState::Frozen { target_anchor },
        ))),
    )
}

fn flatten_with_trigger_guard(
    track: &TrackRuntime,
    strategy_price: f64,
    boundary: BandBoundary,
    trigger_bps: u32,
) -> (Exposure, Option<TrackState>) {
    let target_anchor = protection_target_anchor(track);
    if let TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
        target_anchor: existing_target_anchor,
        boundary: existing_boundary,
    })) = &track.track_state
    {
        if *existing_boundary != boundary {
            return (
                target_anchor.clone(),
                Some(TrackState::Running(ControlState::Automatic(
                    AutoState::FlattenPending {
                        target_anchor,
                        boundary,
                    },
                ))),
            );
        }

        if !strategy::flatten_trigger_price_reached(
            strategy_price,
            trigger_bps,
            track.config.lower_price,
            track.config.upper_price,
            boundary,
        ) {
            return (existing_target_anchor.clone(), None);
        }
    } else if !matches!(
        track.track_state,
        TrackState::Running(ControlState::Automatic(AutoState::Flattening { .. }))
    ) {
        return (
            target_anchor.clone(),
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FlattenPending {
                    target_anchor,
                    boundary,
                },
            ))),
        );
    }

    if matches!(
        track.track_state,
        TrackState::Running(ControlState::Automatic(AutoState::Flattening { .. }))
    ) {
        return (Exposure(0.0), None);
    }
    (
        Exposure(0.0),
        Some(TrackState::Running(ControlState::Automatic(
            AutoState::Flattening { boundary },
        ))),
    )
}

fn protection_target_anchor(track: &TrackRuntime) -> Exposure {
    track
        .desired_exposure
        .clone()
        .unwrap_or_else(|| track.current_exposure.clone())
}

fn effective_runtime_state(track: &TrackRuntime, next: &Option<TrackState>) -> TrackState {
    next.clone().unwrap_or_else(|| track.track_state.clone())
}

fn should_suppress_protected_risk_increase(
    track: &TrackRuntime,
    next_state: &Option<TrackState>,
    approved_target: &Exposure,
) -> bool {
    matches!(
        effective_runtime_state(track, next_state),
        TrackState::Running(ControlState::Automatic(AutoState::Frozen { .. }))
            | TrackState::Running(ControlState::Automatic(AutoState::FlattenPending { .. }))
    ) && approved_target.0.abs() > track.current_exposure.0.abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        AutoState, ControlState, ManualState, TerminationCause, TrackState, TrackStatus,
    };
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
                out_of_band_policy: BandProtectionPolicy::Freeze,
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

    fn runtime_state_from_status(track: &TrackRuntime, status: TrackStatus) -> TrackState {
        let target_anchor = track
            .desired_exposure
            .clone()
            .unwrap_or_else(|| track.current_exposure.clone());
        match status {
            TrackStatus::WaitingMarketData => TrackState::WaitingMarketData,
            TrackStatus::Active => {
                TrackState::Running(ControlState::Automatic(AutoState::FollowingBand))
            }
            TrackStatus::Frozen => {
                TrackState::Running(ControlState::Automatic(AutoState::Frozen { target_anchor }))
            }
            TrackStatus::Flattening => {
                TrackState::Running(ControlState::Automatic(AutoState::Flattening {
                    boundary: BandBoundary::Below,
                }))
            }
            TrackStatus::ManualFlattening => {
                TrackState::Running(ControlState::Manual(ManualState::Flattened))
            }
            TrackStatus::Terminated => TrackState::Terminated {
                cause: TerminationCause::ManualCommand,
            },
            TrackStatus::Paused => TrackState::Paused {
                suspended: ControlState::Automatic(AutoState::FollowingBand),
            },
        }
    }

    fn set_runtime_status(track: &mut TrackRuntime, status: TrackStatus) {
        track.track_state = runtime_state_from_status(track, status);
    }

    fn test_runtime_with_strategy_target(target: Exposure) -> TrackRuntime {
        let mut track = test_runtime();
        track.desired_exposure = Some(target);
        track.track_state = TrackState::Running(ControlState::Automatic(AutoState::FollowingBand));
        track
    }

    fn strategy_target_at(price: f64) -> Exposure {
        match strategy::band_status(price, &test_runtime().config) {
            BandStatus::InBand { target } => target,
            BandStatus::OutOfBand { .. } => panic!("price {price} should be in band"),
        }
    }

    #[test]
    fn reconcile_target_suppresses_execution_when_exposure_unchanged() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(0.0);

        let result = reconcile_target(&track, 100.0);

        assert!(result.suppress_execution);
        assert_eq!(result.desired_exposure, Exposure(0.0));
    }

    #[test]
    fn reconcile_target_terminates_when_risk_requests_termination() {
        let mut track = test_runtime();
        track.current_exposure = Exposure(4.0);
        track.ledger_state.realized_pnl_day =
            Some(chrono::NaiveDate::from_ymd_opt(2026, 4, 8).unwrap());
        track.ledger_state.gross_realized_pnl_today = -90.0;
        track.risk_state.unrealized_pnl = -35.0;
        track.ledger_state.gross_realized_pnl_cumulative = -90.0;
        track.ledger_state.trading_fee_cumulative = 0.0;

        let result = reconcile_target(&track, 95.0);

        assert_eq!(result.desired_exposure, Exposure(0.0));
        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Terminated {
                cause: TerminationCause::Risk(
                    poise_core::risk::RiskTerminationCause::DailyLossLimit,
                ),
            }),
        );
    }

    #[test]
    fn freeze_policy_uses_frozen_without_boundary_guard() {
        let mut track = test_runtime_with_strategy_target(Exposure(4.0));
        track.current_exposure = Exposure(1.0);
        track.config.out_of_band_policy = BandProtectionPolicy::Freeze;

        let result = reconcile_target(&track, 89.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::Frozen {
                    target_anchor: Exposure(4.0),
                }
            ))),
        );
        assert_eq!(result.desired_exposure, Exposure(4.0));
    }

    #[test]
    fn frozen_reentry_clears_target_anchor_and_follows_current_strategy_target() {
        let mut track = test_runtime();
        track.track_state = TrackState::Running(ControlState::Automatic(AutoState::Frozen {
            target_anchor: Exposure(4.0),
        }));
        track.config.out_of_band_policy = BandProtectionPolicy::Freeze;

        let result = reconcile_target(&track, 95.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            ))),
        );
        assert_eq!(result.desired_exposure, strategy_target_at(95.0));
    }

    #[test]
    fn flatten_pending_samples_target_anchor_from_last_risk_approved_target() {
        let mut track = test_runtime_with_strategy_target(Exposure(4.0));
        track.current_exposure = Exposure(1.0);
        track.config.out_of_band_policy = BandProtectionPolicy::Flatten {
            trigger_bps: 500,
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
        };

        let result = reconcile_target(&track, 89.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FlattenPending {
                    target_anchor: Exposure(4.0),
                    boundary: BandBoundary::Below,
                },
            ))),
        );
        assert_eq!(result.desired_exposure, Exposure(4.0));
    }

    #[test]
    fn flatten_pending_keeps_target_anchor_when_price_reenters_band() {
        let mut track = test_runtime_with_strategy_target(Exposure(2.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(4.0),
                boundary: BandBoundary::Below,
            }));
        track.config.out_of_band_policy = BandProtectionPolicy::Flatten {
            trigger_bps: 500,
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
        };

        let result = reconcile_target(&track, 95.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            ))),
        );
        assert_eq!(result.desired_exposure, strategy_target_at(95.0));
    }

    #[test]
    fn reconcile_target_emits_event_when_exposure_changes() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
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
        set_runtime_status(&mut track, TrackStatus::Active);
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
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 89.5);

        assert_eq!(
            result
                .new_runtime_state
                .as_ref()
                .map(|state| state.status()),
            Some(TrackStatus::Frozen),
        );
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_activates_on_first_price() {
        let track = test_runtime();

        let result = reconcile_target(&track, 100.0);

        assert_eq!(
            result
                .new_runtime_state
                .as_ref()
                .map(|state| state.status()),
            Some(TrackStatus::Active),
        );
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_reactivates_after_reenter() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Frozen);
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 100.0);

        assert_eq!(
            result
                .new_runtime_state
                .as_ref()
                .map(|state| state.status()),
            Some(TrackStatus::Active),
        );
        assert!(!result.suppress_execution);
    }

    #[test]
    fn frozen_recovers_to_active_when_price_returns_in_band() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Frozen);
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 100.0);

        assert_eq!(
            result
                .new_runtime_state
                .as_ref()
                .map(|state| state.status()),
            Some(TrackStatus::Active),
        );
    }

    #[test]
    fn flatten_policy_uses_flatten_pending_before_flattening() {
        let mut track = test_runtime();
        track.config.out_of_band_policy = BandProtectionPolicy::Flatten {
            trigger_bps: 500,
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
        };
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.desired_exposure, Exposure(8.0));
        assert_eq!(
            result.new_runtime_state.as_ref().cloned(),
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FlattenPending {
                    target_anchor: Exposure(8.0),
                    boundary: BandBoundary::Below,
                },
            ))),
        );
        assert_eq!(
            result
                .new_runtime_state
                .as_ref()
                .map(|state| state.status()),
            Some(TrackStatus::Frozen),
        );
    }

    #[test]
    fn flatten_pending_rearms_when_price_flips_to_opposite_out_of_band_side() {
        let mut track = test_runtime_with_strategy_target(Exposure(4.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(4.0),
                boundary: BandBoundary::Below,
            }));
        track.config.out_of_band_policy = BandProtectionPolicy::Flatten {
            trigger_bps: 500,
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
        };

        let result = reconcile_target(&track, 111.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FlattenPending {
                    target_anchor: Exposure(4.0),
                    boundary: BandBoundary::Above,
                },
            ))),
        );
        assert_eq!(result.desired_exposure, Exposure(4.0));
    }

    #[test]
    fn flatten_policy_enters_flattening_after_trigger_band_breach_with_current_runtime_shape() {
        let mut track = test_runtime();
        track.config.out_of_band_policy = BandProtectionPolicy::Flatten {
            trigger_bps: 500,
            recover: BandRecoverPolicy::PriceConfirm { bps: 500 },
        };
        set_runtime_status(&mut track, TrackStatus::Frozen);
        track.current_exposure = Exposure(8.0);
        track.desired_exposure = Some(Exposure(8.0));

        let result = reconcile_target(&track, 79.0);

        assert_eq!(
            result
                .new_runtime_state
                .as_ref()
                .map(|state| state.status()),
            Some(TrackStatus::Flattening),
        );
        assert!(result.desired_exposure.0.abs() < 0.001);
    }

    #[test]
    fn reconcile_target_terminate_targets_zero() {
        let mut track = test_runtime();
        track.config.out_of_band_policy = BandProtectionPolicy::Terminate;
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(
            result
                .new_runtime_state
                .as_ref()
                .map(|state| state.status()),
            Some(TrackStatus::Terminated),
        );
        assert!(result.desired_exposure.0.abs() < 0.001);
    }

    #[test]
    fn reconcile_target_emits_risk_cap_event_when_budget_caps_target() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
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
        set_runtime_status(&mut track, TrackStatus::Active);
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
        set_runtime_status(&mut track, TrackStatus::Active);
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
        set_runtime_status(&mut track, TrackStatus::Active);
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
        set_runtime_status(&mut track, TrackStatus::Active);
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
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(4.0);
        track.desired_exposure = Some(Exposure(6.0));
        track.config.out_of_band_policy = BandProtectionPolicy::Freeze;

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.desired_exposure.0, 6.0);
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_uses_budget_from_runtime() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
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
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(1.0);
        track
            .execution_gate_state
            .account_capacity
            .available_notional = Some(0.0);

        let result = reconcile_target(&track, 90.0);

        assert!(result.suppress_execution);
        assert_eq!(
            result.execution_gate_decision,
            ExecutionGateDecision::NoSubmit {
                reason: poise_core::events::ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 2_625.0,
                    available_notional: 0.0,
                },
            }
        );
        assert_eq!(
            result.events,
            vec![DomainEvent::ExecutionGateApplied {
                reason: poise_core::events::ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 2_625.0,
                    available_notional: 0.0,
                },
            }]
        );
    }

    #[test]
    fn margin_guard_reconcile_denies_when_required_notional_exceeds_available_capacity() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(1.0);
        track
            .execution_gate_state
            .account_capacity
            .available_notional = Some(500.0);

        let result = reconcile_target(&track, 90.0);

        assert!(result.suppress_execution);
        assert_eq!(
            result.execution_gate_decision,
            ExecutionGateDecision::NoSubmit {
                reason: poise_core::events::ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 2_625.0,
                    available_notional: 500.0,
                },
            }
        );
    }

    #[test]
    fn margin_guard_reconcile_allows_reduce_only_target() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(5.0);
        track.track_state =
            TrackState::Running(ControlState::Manual(ManualState::TargetOverride {
                target: Exposure(2.0),
            }));
        track
            .execution_gate_state
            .account_capacity
            .available_notional = Some(0.0);

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
