use chrono::{DateTime, Utc};
use poise_core::events::{DomainEvent, ExecutionGateReason};
use poise_core::risk::{self, ExposureIntent, RiskOutcome};
use poise_core::strategy::{self, BandBoundary, BandProtectionPolicy, BandStatus};
use poise_core::types::Exposure;

use crate::execution_gate::{AccountCapacityGate, AccountCapacityGateInput, ExecutionGateDecision};
use crate::execution_plan::DecisionReason;
use crate::loss_guard::build_loss_guard_snapshot_for_asset;
use crate::risk_exposure_gate::{
    self, AppliedRiskAcquisitionRelease, FreshAcquisitionInitialization, RiskAcquisitionRelease,
    RiskExposureGateInput, RiskExposureGateState,
};
use crate::runtime::{
    AppliedRiskCap, AutoState, BandTerminationCause, ControlState, ManualState, TerminationCause,
    TrackRuntime, TrackState,
};

pub struct TargetReconcileResult {
    pub events: Vec<DomainEvent>,
    pub policy_target: Exposure,
    pub risk_approved_target: Exposure,
    pub execution_target: Exposure,
    pub reason: Option<DecisionReason>,
    pub applied_risk_cap: Option<AppliedRiskCap>,
    pub new_runtime_state: Option<TrackState>,
    pub execution_gate_decision: ExecutionGateDecision,
    // Suppresses active pursuit of the target; binding maintenance and gate cleanup may still run.
    pub target_pursuit_suppressed: bool,
    pub acquisition_frontier: Option<Exposure>,
    pub risk_acquisition: Option<RiskAcquisitionRelease>,
    pub fresh_acquisition_initialization: Option<FreshAcquisitionInitialization>,
}

pub fn reconcile_target(track: &TrackRuntime, strategy_price: f64) -> TargetReconcileResult {
    reconcile_target_at(track, strategy_price, Utc::now())
}

pub(crate) struct TargetReconcileWithMetadata {
    pub(crate) result: TargetReconcileResult,
    pub(crate) applied_risk_acquisition_release: Option<AppliedRiskAcquisitionRelease>,
}

pub fn reconcile_target_at(
    track: &TrackRuntime,
    strategy_price: f64,
    observed_at: DateTime<Utc>,
) -> TargetReconcileResult {
    reconcile_target_at_with_metadata(track, strategy_price, observed_at).result
}

pub(crate) fn reconcile_target_at_with_metadata(
    track: &TrackRuntime,
    strategy_price: f64,
    observed_at: DateTime<Utc>,
) -> TargetReconcileWithMetadata {
    if let TrackState::Terminated { .. } = &track.track_state {
        let target = Exposure(0.0);
        let delta = track.current_exposure.delta(&target);
        return reconcile_with_metadata(
            TargetReconcileResult {
                events: exposure_target_change_event(track, &target)
                    .into_iter()
                    .collect(),
                policy_target: target.clone(),
                risk_approved_target: target.clone(),
                execution_target: target,
                reason: delta.is_zero().then_some(DecisionReason::AtExecutionTarget),
                applied_risk_cap: None,
                new_runtime_state: Some(track.track_state.clone()),
                execution_gate_decision: ExecutionGateDecision::Open,
                target_pursuit_suppressed: delta.is_zero(),
                acquisition_frontier: None,
                risk_acquisition: Default::default(),
                fresh_acquisition_initialization: None,
            },
            None,
        );
    }

    if let TrackState::Running(ControlState::Manual(manual_state)) = &track.track_state {
        let target_override = match manual_state {
            ManualState::Flattened => Exposure(0.0),
            ManualState::TargetOverride { target } => target.clone(),
        };
        let delta = track.current_exposure.delta(&target_override);
        return reconcile_with_metadata(
            TargetReconcileResult {
                events: exposure_target_change_event(track, &target_override)
                    .into_iter()
                    .collect(),
                policy_target: target_override.clone(),
                risk_approved_target: target_override.clone(),
                execution_target: target_override,
                reason: delta.is_zero().then_some(DecisionReason::AtExecutionTarget),
                applied_risk_cap: None,
                new_runtime_state: Some(track.track_state.clone()),
                execution_gate_decision: ExecutionGateDecision::Open,
                target_pursuit_suppressed: delta.is_zero(),
                acquisition_frontier: None,
                risk_acquisition: Default::default(),
                fresh_acquisition_initialization: None,
            },
            None,
        );
    }

    let band = strategy::band_status(strategy_price, track.config());

    let (target, new_runtime_state) = match &band {
        BandStatus::InBand { target } => resolve_in_band(track, strategy_price, target),
        BandStatus::OutOfBand { policy, boundary } => {
            apply_out_of_band(track, strategy_price, *policy, *boundary)
        }
    };

    let loss_guard = match build_loss_guard_snapshot_for_asset(
        &track.pnl_stats,
        &track.risk_state,
        Some(track.instrument_spec.settlement_asset().as_str()),
    ) {
        Ok(snapshot) => snapshot,
        Err(_error) => {
            return reconcile_with_metadata(
                pnl_asset_mismatch_result(track, &target, new_runtime_state),
                None,
            );
        }
    };

    let intent = ExposureIntent {
        current: track.current_exposure.clone(),
        target: target.clone(),
        unit_notional: track.config().notional_per_unit,
        loss_guard,
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
            let termination_target = Exposure(0.0);
            let delta = track.current_exposure.delta(&termination_target);
            return reconcile_with_metadata(
                TargetReconcileResult {
                    events: exposure_target_change_event(track, &termination_target)
                        .into_iter()
                        .collect(),
                    policy_target: target,
                    risk_approved_target: termination_target.clone(),
                    execution_target: termination_target,
                    reason: delta.is_zero().then_some(DecisionReason::AtExecutionTarget),
                    applied_risk_cap: None,
                    new_runtime_state: Some(TrackState::Terminated {
                        cause: TerminationCause::Risk(cause),
                    }),
                    execution_gate_decision: ExecutionGateDecision::Open,
                    target_pursuit_suppressed: delta.is_zero(),
                    acquisition_frontier: None,
                    risk_acquisition: Default::default(),
                    fresh_acquisition_initialization: None,
                },
                None,
            );
        }
    };

    let risk_approved_target = approved_target.clone();
    let RiskAcquisitionApplication {
        acquisition_frontier,
        new_runtime_state,
        next_release: risk_acquisition,
        fresh_initialization: fresh_acquisition_initialization,
        applied_release: applied_risk_acquisition_release,
    } = apply_risk_exposure_gate(
        track,
        strategy_price,
        risk_approved_target.clone(),
        new_runtime_state,
        observed_at,
    );
    let execution_target = risk_exposure_gate::execution_target_exposure(
        &track.current_exposure,
        &risk_approved_target,
        acquisition_frontier.as_ref(),
    );

    if should_suppress_protected_risk_increase(track, &new_runtime_state, &execution_target) {
        return reconcile_with_metadata(
            TargetReconcileResult {
                events,
                policy_target: target.clone(),
                risk_approved_target,
                execution_target,
                reason: Some(DecisionReason::BandPolicyBlocked),
                applied_risk_cap,
                new_runtime_state,
                execution_gate_decision: ExecutionGateDecision::Open,
                target_pursuit_suppressed: true,
                acquisition_frontier,
                risk_acquisition,
                fresh_acquisition_initialization,
            },
            applied_risk_acquisition_release,
        );
    }

    let execution_gate_decision = AccountCapacityGate::evaluate(AccountCapacityGateInput {
        current: track.current_exposure.clone(),
        approved_target: execution_target.clone(),
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
        return reconcile_with_metadata(
            TargetReconcileResult {
                events,
                policy_target: target.clone(),
                risk_approved_target,
                execution_target,
                reason: Some(match reason {
                    ExecutionGateReason::AccountCapacityInsufficient { .. } => {
                        DecisionReason::AccountCapacityBlocked
                    }
                    ExecutionGateReason::PnlAssetMismatch { .. } => {
                        DecisionReason::RiskInputInvalid
                    }
                }),
                applied_risk_cap,
                new_runtime_state,
                execution_gate_decision,
                target_pursuit_suppressed: true,
                acquisition_frontier,
                risk_acquisition,
                fresh_acquisition_initialization,
            },
            applied_risk_acquisition_release,
        );
    }

    let delta = track.current_exposure.delta(&execution_target);
    if delta.is_zero() {
        let reason = if execution_target != risk_approved_target {
            DecisionReason::WaitingRiskRelease
        } else {
            DecisionReason::AtExecutionTarget
        };
        return reconcile_with_metadata(
            TargetReconcileResult {
                events,
                policy_target: target.clone(),
                risk_approved_target,
                execution_target,
                reason: Some(reason),
                applied_risk_cap,
                new_runtime_state,
                execution_gate_decision,
                target_pursuit_suppressed: true,
                acquisition_frontier,
                risk_acquisition,
                fresh_acquisition_initialization,
            },
            applied_risk_acquisition_release,
        );
    }

    if let Some(event) = exposure_target_change_event(track, &risk_approved_target) {
        events.push(event);
    }

    reconcile_with_metadata(
        TargetReconcileResult {
            events,
            policy_target: target,
            risk_approved_target,
            execution_target,
            reason: None,
            applied_risk_cap,
            new_runtime_state,
            execution_gate_decision,
            target_pursuit_suppressed: false,
            acquisition_frontier,
            risk_acquisition,
            fresh_acquisition_initialization,
        },
        applied_risk_acquisition_release,
    )
}

fn reconcile_with_metadata(
    result: TargetReconcileResult,
    applied_risk_acquisition_release: Option<AppliedRiskAcquisitionRelease>,
) -> TargetReconcileWithMetadata {
    TargetReconcileWithMetadata {
        result,
        applied_risk_acquisition_release,
    }
}

fn pnl_asset_mismatch_result(
    track: &TrackRuntime,
    policy_target: &Exposure,
    new_runtime_state: Option<TrackState>,
) -> TargetReconcileResult {
    let current = track.current_exposure.clone();
    let reason = ExecutionGateReason::PnlAssetMismatch {
        expected_asset: track
            .instrument_spec
            .settlement_asset()
            .as_str()
            .to_string(),
        actual_asset: track.pnl_stats.pnl_asset.clone().unwrap_or_default(),
    };

    TargetReconcileResult {
        events: vec![DomainEvent::ExecutionGateApplied {
            reason: reason.clone(),
        }],
        policy_target: policy_target.clone(),
        risk_approved_target: current.clone(),
        execution_target: current,
        reason: Some(DecisionReason::RiskInputInvalid),
        applied_risk_cap: None,
        new_runtime_state,
        execution_gate_decision: ExecutionGateDecision::NoSubmit { reason },
        target_pursuit_suppressed: true,
        acquisition_frontier: None,
        risk_acquisition: None,
        fresh_acquisition_initialization: None,
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
    (track
        .inventory_decision()
        .and_then(|decision| decision.applied_risk_cap.as_ref())
        != Some(applied_risk_cap))
    .then_some(DomainEvent::RiskCapApplied {
        intended: applied_risk_cap.intended.clone(),
        capped: applied_risk_cap.capped.clone(),
    })
}

fn apply_risk_exposure_gate(
    track: &TrackRuntime,
    strategy_price: f64,
    approved_target: Exposure,
    new_runtime_state: Option<TrackState>,
    observed_at: DateTime<Utc>,
) -> RiskAcquisitionApplication {
    let effective_state = effective_runtime_state(track, &new_runtime_state);
    let gate_state = match effective_state {
        TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)) => None,
        TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated { gate })) => {
            Some(gate)
        }
        _ => {
            return RiskAcquisitionApplication {
                acquisition_frontier: None,
                new_runtime_state,
                next_release: None,
                fresh_initialization: None,
                applied_release: None,
            };
        }
    };

    let input = RiskExposureGateInput {
        config: track.config().risk_acquisition,
        min_rebalance_units: track.config().min_rebalance_units,
        state: gate_state.clone(),
        current_exposure: track.current_exposure.clone(),
        curve_target: approved_target.clone(),
        strategy_price,
        observed_at,
    };
    let (decision, fresh_acquisition_initialization) =
        if track.fresh_acquisition_pending() && gate_state.is_none() {
            let (decision, initialization) = risk_exposure_gate::initialize_fresh_session(input);
            (decision, Some(initialization))
        } else {
            (risk_exposure_gate::apply(input), None)
        };
    let next_state = merge_gate_state(
        new_runtime_state,
        &track.track_state,
        decision.state.clone(),
    );
    let risk_release_frontier = decision
        .state
        .as_ref()
        .map(|state| state.risk_release_frontier.clone())
        .or_else(|| {
            (decision.risk_release_frontier != approved_target)
                .then_some(decision.risk_release_frontier.clone())
        });

    RiskAcquisitionApplication {
        acquisition_frontier: risk_release_frontier,
        new_runtime_state: next_state,
        next_release: decision.next_release,
        fresh_initialization: fresh_acquisition_initialization,
        applied_release: decision.applied_release,
    }
}

struct RiskAcquisitionApplication {
    acquisition_frontier: Option<Exposure>,
    new_runtime_state: Option<TrackState>,
    next_release: Option<RiskAcquisitionRelease>,
    fresh_initialization: Option<FreshAcquisitionInitialization>,
    applied_release: Option<AppliedRiskAcquisitionRelease>,
}

fn merge_gate_state(
    base_state: Option<TrackState>,
    current_state: &TrackState,
    gate_state: Option<RiskExposureGateState>,
) -> Option<TrackState> {
    match gate_state {
        Some(gate) => Some(TrackState::Running(ControlState::Automatic(
            AutoState::RiskExposureGated { gate },
        ))),
        None => {
            let was_risk_exposure_gated = matches!(
                base_state.as_ref().unwrap_or(current_state),
                TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated { .. }))
            );
            if was_risk_exposure_gated {
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
    let crosses_zero = (track.current_exposure.0 > f64::EPSILON
        && approved_target.0 < -f64::EPSILON)
        || (track.current_exposure.0 < -f64::EPSILON && approved_target.0 > f64::EPSILON);
    matches!(
        effective_runtime_state(track, next_state),
        TrackState::Running(ControlState::Automatic(AutoState::Frozen { .. }))
            | TrackState::Running(ControlState::Automatic(AutoState::FlattenPending { .. }))
    ) && (approved_target.0.abs() > track.current_exposure.0.abs() || crosses_zero)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk_exposure_gate::RiskExposureGateState;
    use crate::runtime::{
        AutoState, ControlState, InventoryDecisionSnapshot, ManualState, TerminationCause,
        TrackState, TrackStatus,
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
            risk_acquisition: Default::default(),
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
            crate::test_instrument::linear_spec(0.1, 0.1, 0.0, 0.0),
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

    fn enable_risk_acquisition(track: &mut TrackRuntime) {
        let mut config = track.config().clone();
        config.risk_acquisition = RiskAcquisitionConfig::default();
        replace_definition(
            track,
            config,
            track.max_notional(),
            track.loss_limits().clone(),
        );
    }

    fn risk_exposure_gate_state(
        risk_release_frontier: f64,
        release_anchor_price: f64,
        release_anchor_target: f64,
    ) -> RiskExposureGateState {
        RiskExposureGateState {
            risk_release_frontier: Exposure(risk_release_frontier),
            release_anchor_price,
            release_anchor_target: Exposure(release_anchor_target),
            stale_since: Utc::now(),
        }
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

    fn fresh_session_runtime(current_exposure: f64) -> TrackRuntime {
        let mut config = test_config();
        config.long_exposure_units = 10.0;
        config.short_exposure_units = 10.0;
        config.risk_acquisition = RiskAcquisitionConfig::default();
        let track = test_runtime_with(config, test_max_notional(), test_loss_limits());
        let instrument_spec = track.instrument_spec.clone();
        track.fresh_start(
            TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)),
            crate::ledger::TrackPnlStats::default(),
            crate::runtime::FreshSessionExternalInputs {
                current_exposure: Exposure(current_exposure),
                position_qty: current_exposure,
                market_data: None,
                instrument_spec,
            },
            Utc.with_ymd_and_hms(2026, 6, 1, 9, 0, 0).unwrap(),
        )
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

        assert!(result.target_pursuit_suppressed);
        assert_eq!(result.risk_approved_target, Exposure(0.0));
    }

    #[test]
    fn risk_acquisition_startup_keeps_desired_and_limits_execution_target() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);

        let result = reconcile_target(&track, 93.75);

        assert_eq!(result.risk_approved_target, Exposure(5.0));
        assert_eq!(result.execution_target, Exposure(2.5));
        assert_eq!(result.acquisition_frontier, Some(Exposure(2.5)));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::RiskExposureGated { .. }
            )))
        ));
        assert!(!result.target_pursuit_suppressed);
        assert!(result.risk_acquisition.is_some());
    }

    #[test]
    fn fresh_session_first_decision_never_grants_new_risk() {
        let observed_at = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let cases = [
            (2.0, 92.0, 8.0, 2.0),
            (8.0, 98.0, 2.0, 2.0),
            (2.0, 104.0, -4.0, 0.0),
            (0.0, 92.0, 8.0, 0.0),
        ];

        for (current, price, approved, expected_execution) in cases {
            let track = fresh_session_runtime(current);

            let result = reconcile_target_at(&track, price, observed_at);

            assert_eq!(result.risk_approved_target, Exposure(approved));
            assert_eq!(
                result.execution_target,
                Exposure(expected_execution),
                "current={current}, approved={approved}"
            );
        }
    }

    #[test]
    fn fresh_session_stale_wait_starts_at_first_valid_observation() {
        let first_observation = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let mut track = fresh_session_runtime(2.0);

        let first = reconcile_target_at(&track, 92.0, first_observation);
        let initialization = first
            .fresh_acquisition_initialization
            .clone()
            .expect("fresh decision should initialize the acquisition session");
        track.track_state = first.new_runtime_state.unwrap();
        track.apply_fresh_acquisition_initialization(Some(initialization));

        let before_stale = reconcile_target_at(
            &track,
            92.0,
            first_observation + chrono::Duration::minutes(59),
        );
        assert_eq!(before_stale.execution_target, Exposure(2.0));

        track.track_state = before_stale.new_runtime_state.unwrap();
        let at_stale = reconcile_target_at(
            &track,
            92.0,
            first_observation + chrono::Duration::minutes(60),
        );
        assert!(at_stale.execution_target.0 > 2.0);
    }

    #[test]
    fn fresh_session_reduction_then_new_episode_uses_initial_ratio() {
        let observed_at = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let mut track = fresh_session_runtime(8.0);

        let reduction = reconcile_target_at(&track, 98.0, observed_at);
        let initialization = reduction
            .fresh_acquisition_initialization
            .clone()
            .expect("fresh reduction should consume the session marker");
        assert_eq!(reduction.execution_target, Exposure(2.0));
        if let Some(new_runtime_state) = reduction.new_runtime_state {
            track.track_state = new_runtime_state;
        }
        track.current_exposure = Exposure(2.0);
        track.apply_fresh_acquisition_initialization(Some(initialization));

        let new_episode =
            reconcile_target_at(&track, 92.0, observed_at + chrono::Duration::minutes(1));

        assert_eq!(new_episode.risk_approved_target, Exposure(8.0));
        assert_eq!(new_episode.execution_target, Exposure(4.0));
        assert!(new_episode.fresh_acquisition_initialization.is_none());
    }

    #[test]
    fn fresh_session_flattening_reentry_keeps_zero_frontier_on_same_price() {
        let observed_at = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let mut track = fresh_session_runtime(2.0);
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::Immediate,
                recover: BandRecoverPolicy::BackInBand,
            },
        );

        let flattening = reconcile_target_at(&track, 80.0, observed_at);
        assert!(flattening.fresh_acquisition_initialization.is_none());
        track.track_state = flattening.new_runtime_state.unwrap();
        track.current_exposure = Exposure(0.0);
        track.desired_exposure = Some(Exposure(0.0));

        let reentry = reconcile_target_at(&track, 92.0, observed_at + chrono::Duration::minutes(1));
        let initialization = reentry
            .fresh_acquisition_initialization
            .clone()
            .expect("first automatic reentry decision should initialize the fresh session");
        assert_eq!(reentry.execution_target, Exposure(0.0));
        track.track_state = reentry.new_runtime_state.unwrap();
        track.apply_fresh_acquisition_initialization(Some(initialization));

        let same_price =
            reconcile_target_at(&track, 92.0, observed_at + chrono::Duration::minutes(2));

        assert_eq!(same_price.risk_approved_target, Exposure(8.0));
        assert_eq!(same_price.execution_target, Exposure(0.0));
        assert!(same_price.fresh_acquisition_initialization.is_none());
    }

    #[test]
    fn non_fresh_flattening_reentry_installs_initial_release_once_when_unfilled() {
        let observed_at = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::Immediate,
                recover: BandRecoverPolicy::BackInBand,
            },
        );
        track.track_state = TrackState::Running(ControlState::Automatic(AutoState::Flattening {
            boundary: BandBoundary::Below,
        }));
        track.current_exposure = Exposure(0.0);
        track.desired_exposure = Some(Exposure(0.0));

        let first = reconcile_target_at_with_metadata(&track, 92.0, observed_at);
        let first_release = first
            .applied_risk_acquisition_release
            .clone()
            .expect("reentry should apply one initial-ratio release");
        let first = first.result;
        assert_eq!(
            first_release.trigger,
            crate::risk_exposure_gate::AppliedRiskAcquisitionReleaseTrigger::InitialRatio
        );
        let Some(TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
            gate,
        }))) = first.new_runtime_state.as_ref()
        else {
            panic!("reentry should install its initial-ratio release gate");
        };
        assert_eq!(gate.risk_release_frontier, first_release.frontier_after);

        track.track_state = first.new_runtime_state.unwrap();
        track.desired_exposure = Some(first.risk_approved_target);
        let second = reconcile_target_at_with_metadata(
            &track,
            92.0,
            observed_at + chrono::Duration::minutes(1),
        );

        assert_eq!(second.applied_risk_acquisition_release, None);
        assert_eq!(
            second.result.acquisition_frontier,
            first.acquisition_frontier
        );
    }

    #[test]
    fn non_fresh_flattening_reentry_preserves_zero_frontier_before_advantage() {
        let observed_at = Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        set_out_of_band_policy(
            &mut track,
            BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::Immediate,
                recover: BandRecoverPolicy::BackInBand,
            },
        );
        track.track_state = TrackState::Running(ControlState::Automatic(AutoState::Flattening {
            boundary: BandBoundary::Below,
        }));
        track.current_exposure = Exposure(1.5);
        track.desired_exposure = Some(Exposure(0.0));

        let first = reconcile_target_at_with_metadata(&track, 100.625, observed_at);

        assert_eq!(first.applied_risk_acquisition_release, None);
        let first = first.result;
        assert_eq!(first.risk_approved_target, Exposure(-0.5));
        assert_eq!(first.execution_target, Exposure(0.0));
        let Some(TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
            gate,
        }))) = first.new_runtime_state.as_ref()
        else {
            panic!("cross-zero reentry should preserve its zero release anchor");
        };
        assert_eq!(gate.risk_release_frontier, Exposure(0.0));

        track.track_state = first.new_runtime_state.unwrap();
        track.desired_exposure = Some(first.risk_approved_target);
        let second = reconcile_target_at_with_metadata(
            &track,
            100.625,
            observed_at + chrono::Duration::minutes(1),
        );

        assert_eq!(second.applied_risk_acquisition_release, None);
        let second = second.result;
        assert_eq!(second.acquisition_frontier, Some(Exposure(0.0)));
        assert_eq!(second.execution_target, Exposure(0.0));

        track.track_state = second.new_runtime_state.unwrap();
        track.current_exposure = Exposure(0.0);
        track.desired_exposure = Some(second.risk_approved_target);
        let after_immediate_flat = reconcile_target_at_with_metadata(
            &track,
            100.625,
            observed_at + chrono::Duration::minutes(1),
        );

        assert_eq!(
            after_immediate_flat.result.acquisition_frontier,
            Some(Exposure(0.0))
        );
        assert_eq!(after_immediate_flat.result.execution_target, Exposure(0.0));
        assert_eq!(after_immediate_flat.applied_risk_acquisition_release, None);
    }

    #[test]
    fn risk_input_invalid_does_not_initialize_fresh_session() {
        let mut track = fresh_session_runtime(2.0);
        track.pnl_stats.pnl_asset = Some("BTC".to_string());

        let result = reconcile_target_at(
            &track,
            92.0,
            Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap(),
        );

        assert_eq!(result.reason, Some(DecisionReason::RiskInputInvalid));
        assert!(result.fresh_acquisition_initialization.is_none());
        assert!(track.fresh_acquisition_pending());
    }

    #[test]
    fn account_capacity_is_evaluated_against_final_execution_target() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track
            .execution_gate_state
            .account_capacity
            .available_notional = Some(1_000.0);

        let result = reconcile_target(&track, 93.75);

        assert_eq!(result.risk_approved_target, Exposure(5.0));
        assert_eq!(result.execution_target, Exposure(2.5));
        assert_eq!(result.execution_gate_decision, ExecutionGateDecision::Open);
        assert!(!result.target_pursuit_suppressed);
    }

    #[test]
    fn risk_acquisition_cross_zero_opposite_residual_waits_before_advantage() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(-0.25);
        track.track_state = TrackState::Running(ControlState::Automatic(AutoState::FollowingBand));

        let result = reconcile_target(&track, 99.375);

        assert_eq!(result.risk_approved_target, Exposure(0.5));
        assert_eq!(result.execution_target, Exposure(0.0));
        assert_eq!(result.acquisition_frontier, Some(Exposure(0.0)));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::RiskExposureGated { .. }
            )))
        ));
        assert!(!result.target_pursuit_suppressed);
    }

    #[test]
    fn risk_acquisition_cross_zero_releases_after_advantage_with_opposite_residual() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(-0.25);
        track.desired_exposure = Some(Exposure(0.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: risk_exposure_gate_state(0.0, 100.0, 0.0),
            }));

        let result = reconcile_target(&track, 98.75);

        assert_eq!(result.risk_approved_target, Exposure(1.0));
        assert_eq!(result.execution_target, Exposure(0.5));
        assert_eq!(result.acquisition_frontier, Some(Exposure(0.5)));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::RiskExposureGated { .. }
            )))
        ));
        assert!(!result.target_pursuit_suppressed);
    }

    #[test]
    fn risk_acquisition_clamps_frontier_when_curve_reenters_inside_frontier() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(1.5);
        track.desired_exposure = Some(Exposure(1.5));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: risk_exposure_gate_state(1.5, 93.75, 5.0),
            }));

        let result = reconcile_target(&track, 98.75);

        assert_eq!(result.risk_approved_target, Exposure(1.0));
        assert_eq!(result.execution_target, Exposure(1.0));
        assert_eq!(result.acquisition_frontier, Some(Exposure(1.0)));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::RiskExposureGated { .. }
            )))
        ));
        assert!(!result.target_pursuit_suppressed);
    }

    #[test]
    fn risk_acquisition_current_past_frontier_does_not_create_reduce_target() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(2.0);
        track.desired_exposure = Some(Exposure(5.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: risk_exposure_gate_state(1.5, 93.75, 5.0),
            }));

        let result = reconcile_target(&track, 93.125);

        assert_eq!(result.risk_approved_target, Exposure(5.5));
        assert_eq!(result.execution_target, Exposure(2.0));
        assert_eq!(result.acquisition_frontier, Some(Exposure(2.0)));
        assert!(result.target_pursuit_suppressed);
        assert_eq!(result.reason, Some(DecisionReason::WaitingRiskRelease));
    }

    #[test]
    fn risk_acquisition_holds_advantage_position_until_desired_crosses_current() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(2.625);
        track.desired_exposure = Some(Exposure(6.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: risk_exposure_gate_state(2.625, 92.5, 6.0),
            }));

        let result = reconcile_target(&track, 93.75);

        assert_eq!(result.risk_approved_target, Exposure(5.0));
        assert_eq!(result.execution_target, Exposure(2.625));
        assert_eq!(result.acquisition_frontier, Some(Exposure(2.625)));
        assert!(result.target_pursuit_suppressed);
    }

    #[test]
    fn risk_acquisition_reduces_when_desired_crosses_current_position() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(3.0);
        track.desired_exposure = Some(Exposure(5.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: risk_exposure_gate_state(1.5, 93.75, 5.0),
            }));

        let result = reconcile_target(&track, 97.5);

        assert_eq!(result.risk_approved_target, Exposure(2.0));
        assert_eq!(result.execution_target, Exposure(2.0));
        assert_eq!(result.acquisition_frontier, Some(Exposure(1.5)));
        assert!(!result.target_pursuit_suppressed);
    }

    #[test]
    fn risk_acquisition_clamps_frontier_after_reduce_reaches_desired() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(2.0);
        track.desired_exposure = Some(Exposure(5.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: risk_exposure_gate_state(1.5, 93.75, 5.0),
            }));

        let result = reconcile_target(&track, 97.5);

        assert_eq!(result.risk_approved_target, Exposure(2.0));
        assert_eq!(result.execution_target, Exposure(2.0));
        assert_eq!(result.acquisition_frontier, Some(Exposure(2.0)));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::RiskExposureGated { .. }
            )))
        ));
        assert!(result.target_pursuit_suppressed);
    }

    #[test]
    fn risk_acquisition_cross_zero_waits_at_flat_before_advantage() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(1.5);
        track.desired_exposure = Some(Exposure(1.5));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: risk_exposure_gate_state(1.5, 93.75, 5.0),
            }));

        let result = reconcile_target(&track, 100.625);

        assert_eq!(result.risk_approved_target, Exposure(-0.5));
        assert_eq!(result.execution_target, Exposure(0.0));
        assert_eq!(result.acquisition_frontier, Some(Exposure(0.0)));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::RiskExposureGated { .. }
            )))
        ));
        assert!(!result.target_pursuit_suppressed);
    }

    #[test]
    fn risk_acquisition_does_not_reinitialize_after_frontier_is_clamped_to_zero() {
        let mut track = test_runtime();
        enable_risk_acquisition(&mut track);
        track.current_exposure = Exposure(0.0);
        track.desired_exposure = Some(Exposure(0.0));
        track.track_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: risk_exposure_gate_state(0.0, 100.0, 0.0),
            }));

        let result = reconcile_target(&track, 99.375);

        assert_eq!(result.risk_approved_target, Exposure(0.5));
        assert_eq!(result.execution_target, Exposure(0.0));
        assert_eq!(result.acquisition_frontier, Some(Exposure(0.0)));
        assert!(result.risk_acquisition.is_some());
        assert!(result.target_pursuit_suppressed);
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

        assert_eq!(result.risk_approved_target, Exposure(0.0));
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
        assert_eq!(result.risk_approved_target, Exposure(4.0));
    }

    #[test]
    fn freeze_policy_identifies_cross_zero_target_as_band_blocked() {
        let mut track = test_runtime_with_strategy_target(Exposure(-0.5));
        track.current_exposure = Exposure(3.0);
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Freeze);

        let result = reconcile_target(&track, 89.0);

        assert!(result.target_pursuit_suppressed);
        assert_eq!(result.reason, Some(DecisionReason::BandPolicyBlocked));
    }

    #[test]
    fn frozen_reentry_clears_target_anchor_and_enters_risk_exposure_gate() {
        let mut track = test_runtime();
        track.track_state = TrackState::Running(ControlState::Automatic(AutoState::Frozen {
            target_anchor: Exposure(4.0),
        }));
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Freeze);

        let result = reconcile_target(&track, 105.0);

        assert_eq!(result.risk_approved_target, strategy_target_at(105.0));
        assert_eq!(result.execution_target, Exposure(-2.0));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::RiskExposureGated { .. }
            )))
        ));
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
        assert_eq!(result.risk_approved_target, Exposure(4.0));
    }

    #[test]
    fn flatten_pending_reentry_clears_target_anchor_and_enters_risk_exposure_gate() {
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

        assert_eq!(result.risk_approved_target, strategy_target_at(105.0));
        assert_eq!(result.execution_target, Exposure(-2.0));
        assert!(matches!(
            result.new_runtime_state,
            Some(TrackState::Running(ControlState::Automatic(
                AutoState::RiskExposureGated { .. }
            )))
        ));
    }

    #[test]
    fn freeze_keeps_sampled_target_anchor_when_risk_cap_changes_approved_target() {
        let mut track = test_runtime_with_strategy_target(Exposure(8.0));
        track.current_exposure = Exposure(4.0);
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Freeze);
        set_max_notional(&mut track, 1500.0);

        let result = reconcile_target(&track, 89.0);

        assert_eq!(result.risk_approved_target, Exposure(4.0));
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

        assert_eq!(result.risk_approved_target, Exposure(4.0));
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

        assert!((result.risk_approved_target.0 - 8.0).abs() < 0.001);
        assert!(!result.target_pursuit_suppressed);
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

        assert_eq!(result.risk_approved_target, Exposure(8.0));
        assert!(!result.target_pursuit_suppressed);
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
        assert!(result.target_pursuit_suppressed);
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
        assert!(result.target_pursuit_suppressed);
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
        assert!(!result.target_pursuit_suppressed);
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

        assert_eq!(result.risk_approved_target, Exposure(8.0));
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
        assert_eq!(result.risk_approved_target, Exposure(4.0));
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
        assert!(result.risk_approved_target.0.abs() < 0.001);
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
        assert!(result.risk_approved_target.0.abs() < 0.001);
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
        assert_eq!(result.risk_approved_target, Exposure(8.0));
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
        assert!(result.risk_approved_target.0.abs() < 0.001);
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

        let below_result = reconcile_target_at_with_metadata(&below_track, 109.5, Utc::now());

        let release = below_result
            .applied_risk_acquisition_release
            .clone()
            .expect("cross-zero reentry should apply the reached advantage release");
        let below_result = below_result.result;
        let Some(TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
            gate,
        }))) = below_result.new_runtime_state.as_ref()
        else {
            panic!("cross-zero reentry should install its applied release gate");
        };
        assert_eq!(gate.risk_release_frontier, release.frontier_after);
        assert_eq!(below_result.risk_approved_target, strategy_target_at(109.5));

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
        assert_eq!(above_result.risk_approved_target, Exposure(0.0));
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
        assert!(result.risk_approved_target.0.abs() < 0.001);
    }

    #[test]
    fn reconcile_target_emits_risk_cap_event_when_budget_caps_target() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(0.0);
        set_max_notional(&mut track, 1500.0);

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.policy_target, Exposure(8.0));
        assert_eq!(result.risk_approved_target, Exposure(4.0));
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

        assert!(result.target_pursuit_suppressed);
        assert_eq!(result.risk_approved_target, Exposure(4.0));
        assert_eq!(result.reason, Some(DecisionReason::AtExecutionTarget));
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

        assert_eq!(result.risk_approved_target, Exposure(4.0));
        assert!(!result.target_pursuit_suppressed);
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
        let applied_risk_cap = AppliedRiskCap {
            intended: Exposure(8.0),
            capped: Exposure(4.0),
        };
        track.apply_inventory_decision(Some(InventoryDecisionSnapshot {
            observed_at: Utc.with_ymd_and_hms(2026, 4, 8, 0, 0, 0).unwrap(),
            policy_target: Exposure(8.0),
            risk_approved_target: Exposure(4.0),
            acquisition_frontier: None,
            execution_target: Exposure(4.0),
            reason: DecisionReason::SubmitPlanned,
            applied_risk_cap: Some(applied_risk_cap.clone()),
            price_execution_gate: crate::price_gate::PriceExecutionGate::Open,
            revision: 1,
            risk_acquisition: None,
        }));
        set_max_notional(&mut track, 1500.0);

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.risk_approved_target, Exposure(4.0));
        assert!(!result.target_pursuit_suppressed);
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::RiskCapApplied { .. }))
        );
        assert_eq!(result.applied_risk_cap, Some(applied_risk_cap));
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

        assert_eq!(result.risk_approved_target, Exposure(0.0));
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
    fn reconcile_holds_when_pnl_asset_mismatches_settlement_asset() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(2.0);
        track.pnl_stats.pnl_asset = Some("BTC".to_string());

        let result = reconcile_target(&track, 90.0);

        assert!(result.target_pursuit_suppressed);
        assert_eq!(result.risk_approved_target, Exposure(2.0));
        assert_eq!(
            result.execution_gate_decision,
            ExecutionGateDecision::NoSubmit {
                reason: ExecutionGateReason::PnlAssetMismatch {
                    expected_asset: "USDT".to_string(),
                    actual_asset: "BTC".to_string(),
                }
            }
        );
        assert_eq!(result.reason, Some(DecisionReason::RiskInputInvalid));
        assert!(result.events.iter().any(|event| matches!(
            event,
            DomainEvent::ExecutionGateApplied {
                reason: ExecutionGateReason::PnlAssetMismatch { .. }
            }
        )));
    }

    #[test]
    fn freeze_keeps_last_in_band_target_instead_of_current_exposure() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(4.0);
        track.desired_exposure = Some(Exposure(6.0));
        set_out_of_band_policy(&mut track, BandProtectionPolicy::Freeze);

        let result = reconcile_target(&track, 85.0);

        assert_eq!(result.risk_approved_target.0, 6.0);
        assert!(result.target_pursuit_suppressed);
        assert_eq!(result.reason, Some(DecisionReason::BandPolicyBlocked));
    }

    #[test]
    fn reconcile_target_uses_budget_from_runtime() {
        let mut track = test_runtime();
        set_runtime_status(&mut track, TrackStatus::Active);
        track.current_exposure = Exposure(0.0);
        set_max_notional(&mut track, 1500.0);

        let result = reconcile_target(&track, 90.0);

        assert_eq!(result.risk_approved_target, Exposure(4.0));
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

        assert!(result.target_pursuit_suppressed);
        assert_eq!(
            result.execution_gate_decision,
            ExecutionGateDecision::NoSubmit {
                reason: poise_core::events::ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 1_125.0,
                    available_notional: 0.0,
                },
            }
        );
        assert_eq!(
            result.events,
            vec![DomainEvent::ExecutionGateApplied {
                reason: poise_core::events::ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 1_125.0,
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

        assert!(result.target_pursuit_suppressed);
        assert_eq!(
            result.execution_gate_decision,
            ExecutionGateDecision::NoSubmit {
                reason: poise_core::events::ExecutionGateReason::AccountCapacityInsufficient {
                    required_notional: 1_125.0,
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

        assert!(!result.target_pursuit_suppressed);
        assert_eq!(result.risk_approved_target, Exposure(2.0));
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ExposureTargetChanged { .. }))
        );
    }
}
