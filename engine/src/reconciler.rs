use poise_core::events::DomainEvent;
use poise_core::risk::{self, ExposureIntent, RiskOutcome};
use poise_core::strategy::{self, BandBoundary, BandProtectionPolicy, BandStatus};
use poise_core::types::Exposure;

use crate::execution_gate::{AccountCapacityGate, AccountCapacityGateInput, ExecutionGateDecision};
use crate::loss_guard::build_loss_guard_snapshot;
use crate::risk_exposure_gate::{
    self, RiskAcquisitionRelease, RiskExposureGateInput, RiskExposureGateState,
};
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
    pub risk_acquisition: Option<RiskAcquisitionRelease>,
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
            risk_acquisition: None,
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
            risk_acquisition: None,
        };
    }

    let band = strategy::band_status(strategy_price, track.config());

    let (target, new_runtime_state) = match &band {
        BandStatus::InBand { target } => resolve_in_band(track, strategy_price, target),
        BandStatus::OutOfBand { policy, boundary } => {
            apply_out_of_band(track, strategy_price, *policy, *boundary)
        }
    };

    let intent = ExposureIntent {
        current: track.current_exposure.clone(),
        target: target.clone(),
        unit_notional: track.config().notional_per_unit,
        loss_guard: build_loss_guard_snapshot(&track.pnl_stats, &track.risk_state),
    };

    let decision = risk::evaluate_risk_outcome(&intent, track.max_notional(), track.loss_limits());

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
                risk_acquisition: None,
            };
        }
    };

    let (approved_target, new_runtime_state, risk_acquisition) =
        apply_risk_exposure_gate(track, strategy_price, approved_target, new_runtime_state);

    if should_suppress_protected_risk_increase(track, &new_runtime_state, &approved_target) {
        return TargetReconcileResult {
            events,
            desired_exposure: approved_target,
            applied_risk_cap,
            new_runtime_state,
            execution_gate_decision: ExecutionGateDecision::Open,
            suppress_execution: true,
            risk_acquisition,
        };
    }

    let execution_gate_decision = AccountCapacityGate::evaluate(AccountCapacityGateInput {
        current: track.current_exposure.clone(),
        approved_target: approved_target.clone(),
        unit_notional: track.config().notional_per_unit,
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
            risk_acquisition,
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
            risk_acquisition,
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
        risk_acquisition,
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

fn apply_risk_exposure_gate(
    track: &TrackRuntime,
    strategy_price: f64,
    approved_target: Exposure,
    new_runtime_state: Option<TrackState>,
) -> (Exposure, Option<TrackState>, Option<RiskAcquisitionRelease>) {
    if track.config().risk_increase_delay.is_none() {
        return (approved_target, new_runtime_state, None);
    }

    let effective_state = effective_runtime_state(track, &new_runtime_state);
    let gate_state = match effective_state {
        TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)) => None,
        TrackState::Running(ControlState::Automatic(AutoState::AcquiringRiskExposure { gate })) => {
            Some(gate)
        }
        _ => return (approved_target, new_runtime_state, None),
    };

    let decision = risk_exposure_gate::apply(RiskExposureGateInput {
        config: track.config().risk_increase_delay,
        min_rebalance_units: track.config().min_rebalance_units,
        state: gate_state,
        current_exposure: track.current_exposure.clone(),
        curve_target: approved_target,
        strategy_price,
    });
    let next_state = merge_gate_state(
        new_runtime_state,
        &track.track_state,
        decision.state.clone(),
    );

    (decision.allowed_target, next_state, decision.next_release)
}

fn merge_gate_state(
    base_state: Option<TrackState>,
    current_state: &TrackState,
    gate_state: Option<RiskExposureGateState>,
) -> Option<TrackState> {
    match gate_state {
        Some(gate) => Some(TrackState::Running(ControlState::Automatic(
            AutoState::AcquiringRiskExposure { gate },
        ))),
        None => {
            let was_acquiring = matches!(
                base_state.as_ref().unwrap_or(current_state),
                TrackState::Running(ControlState::Automatic(
                    AutoState::AcquiringRiskExposure { .. }
                ))
            );
            if was_acquiring {
                Some(TrackState::Running(ControlState::Automatic(
                    AutoState::FollowingBand,
                )))
            } else {
                base_state
            }
        }
    }
}

fn resolve_in_band(
    track: &TrackRuntime,
    strategy_price: f64,
    target: &Exposure,
) -> (Exposure, Option<TrackState>) {
    match &track.track_state {
        TrackState::WaitingMarketData => (
            target.clone(),
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            ))),
        ),
        TrackState::Running(ControlState::Automatic(AutoState::Frozen { .. }))
        | TrackState::Running(ControlState::Automatic(AutoState::FlattenPending { .. })) => (
            target.clone(),
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            ))),
        ),
        TrackState::Running(ControlState::Automatic(AutoState::Flattening { boundary })) => {
            let BandProtectionPolicy::Flatten { recover, .. } = track.config().out_of_band_policy
            else {
                return (
                    target.clone(),
                    Some(TrackState::Running(ControlState::Automatic(
                        AutoState::FollowingBand,
                    ))),
                );
            };
            if strategy::band_reentry_confirmed(
                strategy_price,
                &recover,
                track.config().lower_price,
                track.config().upper_price,
                *boundary,
            ) {
                (
                    target.clone(),
                    Some(TrackState::Running(ControlState::Automatic(
                        AutoState::FollowingBand,
                    ))),
                )
            } else {
                (Exposure(0.0), None)
            }
        }
        _ => (target.clone(), None),
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
        BandProtectionPolicy::Flatten { trigger, .. } => {
            flatten_with_trigger(track, strategy_price, boundary, trigger)
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

fn flatten_with_trigger(
    track: &TrackRuntime,
    strategy_price: f64,
    boundary: BandBoundary,
    trigger: strategy::BandFlattenTrigger,
) -> (Exposure, Option<TrackState>) {
    if matches!(trigger, strategy::BandFlattenTrigger::Immediate) {
        if matches!(
            track.track_state,
            TrackState::Running(ControlState::Automatic(AutoState::Flattening { .. }))
        ) {
            return (Exposure(0.0), None);
        }
        return (
            Exposure(0.0),
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::Flattening { boundary },
            ))),
        );
    }

    let strategy::BandFlattenTrigger::FlattenConfirm { bps: confirm_bps } = trigger else {
        unreachable!();
    };

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

        if !strategy::flatten_confirm_reached(
            strategy_price,
            confirm_bps,
            track.config().lower_price,
            track.config().upper_price,
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
    use crate::risk_exposure_gate::RiskExposureGateState;
    use crate::runtime::{
        AutoState, ControlState, ManualState, TerminationCause, TrackState, TrackStatus,
    };
    use chrono::{TimeZone, Utc};
    use poise_core::risk::LossLimits;
    use poise_core::strategy::*;
    use poise_core::track::{Instrument, TrackDefinition, Venue};

    fn test_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_increase_delay: None,
        }
    }

    fn test_runtime() -> TrackRuntime {
        test_runtime_with(test_config(), test_max_notional(), test_loss_limits())
    }

    fn test_runtime_with(
        config: TrackConfig,
        max_notional: f64,
        loss_limits: LossLimits,
    ) -> TrackRuntime {
        let definition = TrackDefinition::try_new(
            "test".into(),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            config,
            Some(max_notional),
            loss_limits,
            None,
        )
        .unwrap();

        TrackRuntime::new(
            definition,
            poise_core::types::ExchangeRules {
                price_tick: 0.1,
                price_precision: Default::default(),
                quantity_step: 0.1,
                min_qty: 0.0,
                min_notional: 0.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        )
    }

    fn replace_definition(
        track: &mut TrackRuntime,
        config: TrackConfig,
        max_notional: f64,
        loss_limits: LossLimits,
    ) {
        track.replace_definition_for_test(config, max_notional, loss_limits);
    }

    fn set_out_of_band_policy(track: &mut TrackRuntime, policy: BandProtectionPolicy) {
        let mut config = track.config().clone();
        config.out_of_band_policy = policy;
        replace_definition(
            track,
            config,
            track.max_notional(),
            track.loss_limits().clone(),
        );
    }

    fn set_max_notional(track: &mut TrackRuntime, max_notional: f64) {
        replace_definition(
            track,
            track.config().clone(),
            max_notional,
            track.loss_limits().clone(),
        );
    }

    fn set_loss_limits(track: &mut TrackRuntime, loss_limits: LossLimits) {
        replace_definition(
            track,
            track.config().clone(),
            track.max_notional(),
            loss_limits,
        );
    }

    fn enable_risk_increase_delay(track: &mut TrackRuntime) {
        let mut config = track.config().clone();
        config.risk_increase_delay = Some(RiskIncreaseDelayConfig::default());
        replace_definition(
            track,
            config,
            track.max_notional(),
            track.loss_limits().clone(),
        );
    }

    fn test_max_notional() -> f64 {
        3000.0
    }

    fn test_loss_limits() -> LossLimits {
        LossLimits {
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
        match strategy::band_status(price, test_runtime().config()) {
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
    fn risk_increase_delay_startup_exposes_allowed_target_not_curve_target() {
        let mut track = test_runtime();
        enable_risk_increase_delay(&mut track);

        let result = reconcile_target(&track, 93.75);

        assert_eq!(result.desired_exposure, Exposure(1.5));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::AcquiringRiskExposure { .. }
            )))
        ));
        assert!(!result.suppress_execution);
        assert!(result.risk_acquisition.is_some());
    }

    #[test]
    fn risk_increase_delay_reduces_allowed_target_when_curve_reenters_inside() {
        let mut track = test_runtime();
        enable_risk_increase_delay(&mut track);
        track.current_exposure = Exposure(1.5);
        track.desired_exposure = Some(Exposure(1.5));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::AcquiringRiskExposure {
                gate: RiskExposureGateState {
                    allowed_target: Exposure(1.5),
                    anchor_price: 93.75,
                    anchor_curve_target: Exposure(5.0),
                },
            }));

        let result = reconcile_target(&track, 98.75);

        assert_eq!(result.desired_exposure, Exposure(1.0));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand
            )))
        ));
        assert!(!result.suppress_execution);
    }

    #[test]
    fn risk_increase_delay_cross_zero_reduces_to_flat_first() {
        let mut track = test_runtime();
        enable_risk_increase_delay(&mut track);
        track.current_exposure = Exposure(1.5);
        track.desired_exposure = Some(Exposure(1.5));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::AcquiringRiskExposure {
                gate: RiskExposureGateState {
                    allowed_target: Exposure(1.5),
                    anchor_price: 93.75,
                    anchor_curve_target: Exposure(5.0),
                },
            }));

        let result = reconcile_target(&track, 101.25);

        assert_eq!(result.desired_exposure, Exposure(0.0));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand
            )))
        ));
        assert!(!result.suppress_execution);
    }

    #[test]
    fn reconcile_target_terminates_when_risk_requests_termination() {
        let mut track = test_runtime();
        track.current_exposure = Exposure(4.0);
        track.pnl_stats.pnl_utc_day = chrono::NaiveDate::from_ymd_opt(2026, 4, 8).unwrap();
        track.pnl_stats.gross_realized_pnl_today = -90.0;
        track.risk_state.unrealized_pnl = -35.0;
        track.pnl_stats.gross_realized_pnl_cumulative = -90.0;
        track.pnl_stats.trading_fee_cumulative = 0.0;

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
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Freeze);

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
    fn frozen_reentry_clears_target_anchor_and_follows_current_strategy_target_on_reentry_tick() {
        let mut track = test_runtime();
        track.track_state = TrackState::Running(ControlState::Automatic(AutoState::Frozen {
            target_anchor: Exposure(4.0),
        }));
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Freeze);

        let result = reconcile_target(&track, 105.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            ))),
        );
        assert_eq!(result.desired_exposure, strategy_target_at(105.0));
    }

    #[test]
    fn flatten_pending_samples_target_anchor_from_last_risk_approved_target() {
        let mut track = test_runtime_with_strategy_target(Exposure(4.0));
        track.current_exposure = Exposure(1.0);
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );

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
    fn flatten_pending_reentry_clears_target_anchor_and_follows_current_strategy_target() {
        let mut track = test_runtime_with_strategy_target(Exposure(2.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(4.0),
                boundary: BandBoundary::Below,
            }));
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );

        let result = reconcile_target(&track, 105.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            ))),
        );
        assert_eq!(result.desired_exposure, strategy_target_at(105.0));
    }

    #[test]
    fn freeze_keeps_sampled_target_anchor_when_risk_cap_changes_approved_target() {
        let mut track = test_runtime_with_strategy_target(Exposure(8.0));
        track.current_exposure = Exposure(4.0);
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Freeze);
        set_max_notional(&mut track, 1500.0);

        let result = reconcile_target(&track, 89.0);

        assert_eq!(result.desired_exposure, Exposure(4.0));
        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::Frozen {
                    target_anchor: Exposure(8.0),
                }
            ))),
        );
    }

    #[test]
    fn flatten_pending_keeps_sampled_target_anchor_when_risk_cap_changes_approved_target() {
        let mut track = test_runtime_with_strategy_target(Exposure(8.0));
        track.current_exposure = Exposure(4.0);
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );
        set_max_notional(&mut track, 1500.0);

        let result = reconcile_target(&track, 89.0);

        assert_eq!(result.desired_exposure, Exposure(4.0));
        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FlattenPending {
                    target_anchor: Exposure(8.0),
                    boundary: BandBoundary::Below,
                },
            ))),
        );
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
    fn flatten_policy_uses_flatten_confirm_before_flattening_with_current_runtime_shape() {
        let mut track = test_runtime();
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );
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
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );

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
    fn flatten_policy_immediate_trigger_enters_flattening_with_current_runtime_shape() {
        let mut track = test_runtime();
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::Immediate,
                recover: BandRecoverPolicy::BackInBand,
            },
        );
        set_runtime_status(&mut track, TrackStatus::Active);
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
    fn flatten_policy_immediate_trigger_bypasses_flatten_pending() {
        let mut track = test_runtime();
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::Immediate,
                recover: BandRecoverPolicy::BackInBand,
            },
        );
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(8.0);
        track.desired_exposure = Some(Exposure(8.0));

        let result = reconcile_target(&track, 79.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::Flattening {
                    boundary: BandBoundary::Below,
                },
            ))),
        );
        assert!(!matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FlattenPending { .. },
            )))
        ));
        assert!(result.desired_exposure.0.abs() < 0.001);
    }

    #[test]
    fn flatten_policy_uses_flatten_pending_before_flattening_when_trigger_is_flatten_confirm() {
        let mut track = test_runtime();
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(8.0);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FlattenPending {
                    target_anchor: Exposure(8.0),
                    boundary: BandBoundary::Below,
                },
            ))),
        );
        assert_eq!(result.desired_exposure, Exposure(8.0));
    }

    #[test]
    fn flatten_policy_uses_flatten_confirm_before_flattening_second_tick() {
        let mut track = test_runtime();
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(8.0),
                boundary: BandBoundary::Below,
            }));
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
    fn flattening_reentry_confirm_recovery_is_boundary_specific() {
        let mut below_track = test_runtime();
        set_out_of_band_policy(
            &mut below_track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );
        below_track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::Flattening {
                boundary: BandBoundary::Below,
            }));
        below_track.current_exposure = Exposure(8.0);

        let below_result = reconcile_target(&below_track, 109.5);

        assert_eq!(
            below_result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            ))),
        );
        assert_eq!(below_result.desired_exposure, strategy_target_at(109.5));

        let mut above_track = test_runtime();
        set_out_of_band_policy(
            &mut above_track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
        );
        above_track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::Flattening {
                boundary: BandBoundary::Above,
            }));
        above_track.current_exposure = Exposure(-8.0);

        let above_result = reconcile_target(&above_track, 109.5);

        assert_eq!(above_result.new_runtime_state, None);
        assert_eq!(above_result.desired_exposure, Exposure(0.0));
    }

    #[test]
    fn reconcile_target_terminate_targets_zero() {
        let mut track = test_runtime();
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Terminate);
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
        set_max_notional(&mut track, 1500.0);

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
        set_max_notional(&mut track, 1500.0);

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
        set_max_notional(&mut track, 1500.0);

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
        set_max_notional(&mut track, 1500.0);

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
    fn reconcile_builds_loss_guard_snapshot_from_pnl_stats() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(2.0);
        track.pnl_stats.pnl_utc_day = chrono::NaiveDate::from_ymd_opt(2026, 4, 8).unwrap();
        track.pnl_stats.gross_realized_pnl_today = 100.0;
        track.pnl_stats.gross_realized_pnl_cumulative = 320.0;
        track.pnl_stats.trading_fee_today = 8.0;
        track.pnl_stats.trading_fee_cumulative = 20.0;
        track.pnl_stats.funding_fee_today = -2.0;
        track.pnl_stats.funding_fee_cumulative = -5.0;
        track.risk_state.unrealized_pnl = -215.0;
        set_loss_limits(
            &mut track,
            LossLimits {
                daily_loss_limit: 120.0,
                total_loss_limit: 500.0,
            },
        );

        let result = reconcile_target(&track, 90.0);

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
    fn freeze_keeps_last_in_band_target_instead_of_current_exposure() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(4.0);
        track.desired_exposure = Some(Exposure(6.0));
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Freeze);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.desired_exposure.0, 6.0);
        assert!(result.suppress_execution);
    }

    #[test]
    fn reconcile_target_uses_budget_from_runtime() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(0.0);
        set_max_notional(&mut track, 1500.0);

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
