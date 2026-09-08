//! Read-only explanation of the consequences of replacing a track definition.

use chrono::{DateTime, Utc};
use poise_core::instrument::{Amount, InstrumentSpec, PositionQty};
use poise_core::risk::{
    ExposureIntent, LossGuardSnapshot, RiskOutcome, RiskTerminationCause, evaluate_risk_outcome,
};
use poise_core::strategy::{BandFlattenTrigger, BandProtectionPolicy, BandStatus, band_status};
use poise_core::track::TrackDefinition;
use poise_core::types::Exposure;

use crate::risk_exposure_gate::{
    RiskExposureGateInput, execution_target_exposure, initialize_fresh_session,
};
use crate::runtime::{exposure_from_position_qty, position_qty_per_exposure_unit};

/// The immutable facts needed to explain a possible replacement definition.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigurationPreviewInput {
    pub active: TrackDefinition,
    /// The active runtime's last committed policy target, if a decision exists.
    pub active_policy_target: Option<Exposure>,
    pub candidate: TrackDefinition,
    pub instrument_spec: InstrumentSpec,
    pub current_position_qty: PositionQty,
    pub loss_guard: LossGuardSnapshot,
    pub strategy_price: f64,
    pub observed_at: DateTime<Utc>,
    /// Explicit account capacity available for increasing exposure, in the
    /// instrument's notional asset.
    pub max_increase_notional: f64,
}

/// A condition that prevents a candidate configuration from being applied as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationPreviewBlocker {
    InstrumentMismatch,
    VenueMinimum,
    InsufficientIncreaseNotional,
}

/// Invalid data supplied to the public preview seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigurationPreviewError {
    InvalidStrategyPrice,
    InvalidMaxIncreaseNotional,
    InvalidInstrumentSpec(String),
}

/// The fresh automatic state implied by the candidate's current band policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationPolicyState {
    InBand,
    Frozen,
    Flattening,
    FlattenPending,
    Terminated,
}

/// The candidate's risk decision after its policy target is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationRiskOutcome {
    Allowed,
    Capped,
    DailyLossTerminated,
    TotalLossTerminated,
}

/// A read-only calculation. Exposure-unit values are deliberately kept in their
/// own active/candidate scales; only native position quantities and notionals are
/// compared across definitions.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigurationPreview {
    pub active_current_exposure: Exposure,
    pub candidate_current_exposure: Exposure,
    pub active_policy_target: Option<Exposure>,
    pub candidate_curve_target: Exposure,
    pub candidate_policy_target: Exposure,
    pub candidate_policy_state: ConfigurationPolicyState,
    pub candidate_risk_approved_target: Exposure,
    pub candidate_risk_outcome: ConfigurationRiskOutcome,
    pub active_policy_target_position_qty: Option<PositionQty>,
    pub candidate_policy_target_position_qty: PositionQty,
    pub target_position_qty_delta: Option<PositionQty>,
    pub active_policy_target_signed_notional: Option<Amount>,
    pub candidate_policy_target_signed_notional: Amount,
    pub target_signed_notional_delta: Option<Amount>,
    pub candidate_position_qty_per_unit: f64,
    pub candidate_target_delta_from_current_exposure: Exposure,
    pub candidate_position_qty_delta_from_current: PositionQty,
    pub minimum_rebalance_raw_quantity: PositionQty,
    pub minimum_rebalance_raw_notional: Amount,
    pub minimum_rebalance_quantity: PositionQty,
    pub minimum_rebalance_notional: Amount,
    pub maximum_target_position_qty: PositionQty,
    pub required_increase_notional: f64,
    pub available_increase_notional: f64,
    pub initial_frontier: Exposure,
    pub initial_execution_target: Exposure,
    pub blockers: Vec<ConfigurationPreviewBlocker>,
}

/// Explains the candidate using the same unit conversion and fresh-session gate
/// semantics as the runtime. This function is side-effect free and does not
/// represent an apply decision.
pub fn preview_configuration(
    input: ConfigurationPreviewInput,
) -> Result<ConfigurationPreview, ConfigurationPreviewError> {
    if !input.strategy_price.is_finite() || input.strategy_price <= f64::EPSILON {
        return Err(ConfigurationPreviewError::InvalidStrategyPrice);
    }
    if !input.max_increase_notional.is_finite() || input.max_increase_notional < 0.0 {
        return Err(ConfigurationPreviewError::InvalidMaxIncreaseNotional);
    }
    input
        .instrument_spec
        .validate()
        .map_err(ConfigurationPreviewError::InvalidInstrumentSpec)?;
    let active_config = input.active.track_config();
    let candidate_config = input.candidate.track_config();
    let active_current_exposure = exposure_from_position_qty(
        active_config,
        &input.instrument_spec,
        input.current_position_qty,
    );
    let candidate_current_exposure = exposure_from_position_qty(
        candidate_config,
        &input.instrument_spec,
        input.current_position_qty,
    );
    let active_policy_target = input.active_policy_target.clone();
    let (candidate_curve_target, candidate_policy_target, candidate_policy_state) =
        candidate_policy_target(
            candidate_config,
            candidate_current_exposure.clone(),
            input.strategy_price,
        );
    let (candidate_risk_approved_target, candidate_risk_outcome) = candidate_risk_outcome(
        &input.candidate,
        candidate_current_exposure.clone(),
        candidate_policy_target.clone(),
        input.loss_guard.clone(),
    );

    let active_position_qty_per_unit =
        position_qty_per_exposure_unit(active_config, &input.instrument_spec);
    let candidate_position_qty_per_unit =
        position_qty_per_exposure_unit(candidate_config, &input.instrument_spec);
    let active_policy_target_position_qty = active_policy_target
        .clone()
        .map(|target| quantity_for_exposure(target, active_position_qty_per_unit));
    let candidate_policy_target_position_qty = quantity_for_exposure(
        candidate_policy_target.clone(),
        candidate_position_qty_per_unit,
    );
    let target_position_qty_delta = active_policy_target_position_qty.map(|active_quantity| {
        position_qty(candidate_policy_target_position_qty.value() - active_quantity.value())
    });
    let active_policy_target_signed_notional = active_policy_target_position_qty
        .map(|quantity| signed_notional(&input.instrument_spec, quantity, input.strategy_price));
    let candidate_policy_target_signed_notional = signed_notional(
        &input.instrument_spec,
        candidate_policy_target_position_qty,
        input.strategy_price,
    );
    let target_signed_notional_delta =
        active_policy_target_signed_notional
            .as_ref()
            .map(|active_notional| {
                amount(
                    candidate_policy_target_signed_notional.value() - active_notional.value(),
                    &input.instrument_spec,
                )
            });

    let candidate_target_delta_from_current_exposure =
        Exposure(candidate_policy_target.0 - candidate_current_exposure.0);
    let candidate_position_qty_delta_from_current = position_qty(
        candidate_policy_target_position_qty.value() - input.current_position_qty.value(),
    );
    let minimum_rebalance_raw_quantity =
        position_qty(candidate_config.min_rebalance_units * candidate_position_qty_per_unit);
    let minimum_rebalance_raw_notional = unsigned_notional(
        &input.instrument_spec,
        minimum_rebalance_raw_quantity,
        input.strategy_price,
    );
    let minimum_rebalance_quantity = input
        .instrument_spec
        .normalize_position_qty(minimum_rebalance_raw_quantity);
    let minimum_rebalance_notional = unsigned_notional(
        &input.instrument_spec,
        minimum_rebalance_quantity,
        input.strategy_price,
    );
    let maximum_target_position_qty = input.instrument_spec.normalize_position_qty(
        input
            .instrument_spec
            .position_qty_for_notional(
                amount(
                    input.candidate.effective_max_notional(),
                    &input.instrument_spec,
                ),
                candidate_config.band_center(),
            )
            .expect(
                "validated candidate definition and instrument spec must convert maximum notional",
            ),
    );

    let current_absolute_notional = unsigned_notional(
        &input.instrument_spec,
        input.current_position_qty,
        input.strategy_price,
    )
    .value();
    let candidate_increases_risk_limit = input.candidate.effective_max_notional()
        > input.active.effective_max_notional() + f64::EPSILON;
    let required_increase_notional = candidate_increases_risk_limit
        .then(|| (input.candidate.effective_max_notional() - current_absolute_notional).max(0.0))
        .unwrap_or(0.0);

    let gate_input = RiskExposureGateInput {
        config: candidate_config.risk_acquisition,
        min_rebalance_units: candidate_config.min_rebalance_units,
        state: None,
        current_exposure: candidate_current_exposure.clone(),
        curve_target: candidate_risk_approved_target.clone(),
        strategy_price: input.strategy_price,
        observed_at: input.observed_at,
    };
    let (gate_decision, initialization) = initialize_fresh_session(gate_input);
    let initial_execution_target = execution_target_exposure(
        &candidate_current_exposure,
        &candidate_risk_approved_target,
        Some(&gate_decision.risk_release_frontier),
    );

    let mut blockers = Vec::new();
    if input.active.instrument() != input.instrument_spec.instrument()
        || input.candidate.instrument() != input.instrument_spec.instrument()
    {
        blockers.push(ConfigurationPreviewBlocker::InstrumentMismatch);
    }
    let meets_venue_minimum = minimum_rebalance_quantity.value() > f64::EPSILON
        && input
            .instrument_spec
            .meets_minimum(minimum_rebalance_quantity, input.strategy_price)
            .expect("validated instrument spec and strategy price must evaluate venue minimum");
    let maximum_target_is_executable = maximum_target_position_qty.value() > f64::EPSILON
        && input
            .instrument_spec
            .meets_minimum(maximum_target_position_qty, input.strategy_price)
            .expect("validated instrument spec and strategy price must evaluate venue minimum");
    if !meets_venue_minimum || !maximum_target_is_executable {
        blockers.push(ConfigurationPreviewBlocker::VenueMinimum);
    }
    if required_increase_notional > input.max_increase_notional + f64::EPSILON {
        blockers.push(ConfigurationPreviewBlocker::InsufficientIncreaseNotional);
    }

    Ok(ConfigurationPreview {
        active_current_exposure,
        candidate_current_exposure,
        active_policy_target,
        candidate_curve_target,
        candidate_policy_target,
        candidate_policy_state,
        candidate_risk_approved_target,
        candidate_risk_outcome,
        active_policy_target_position_qty,
        candidate_policy_target_position_qty,
        target_position_qty_delta,
        active_policy_target_signed_notional,
        candidate_policy_target_signed_notional,
        target_signed_notional_delta,
        candidate_position_qty_per_unit,
        candidate_target_delta_from_current_exposure,
        candidate_position_qty_delta_from_current,
        minimum_rebalance_raw_quantity,
        minimum_rebalance_raw_notional,
        minimum_rebalance_quantity,
        minimum_rebalance_notional,
        maximum_target_position_qty,
        required_increase_notional,
        available_increase_notional: input.max_increase_notional,
        initial_frontier: initialization.initial_frontier,
        initial_execution_target,
        blockers,
    })
}

fn candidate_policy_target(
    config: &poise_core::strategy::TrackConfig,
    current_exposure: Exposure,
    strategy_price: f64,
) -> (Exposure, Exposure, ConfigurationPolicyState) {
    match band_status(strategy_price, config) {
        BandStatus::InBand { target } => (target.clone(), target, ConfigurationPolicyState::InBand),
        BandStatus::OutOfBand { policy, .. } => {
            let curve_target = poise_core::strategy::desired_exposure(strategy_price, config);
            match policy {
                BandProtectionPolicy::Freeze => (
                    curve_target,
                    current_exposure,
                    ConfigurationPolicyState::Frozen,
                ),
                BandProtectionPolicy::Flatten {
                    trigger: BandFlattenTrigger::Immediate,
                    ..
                } => (
                    curve_target,
                    Exposure(0.0),
                    ConfigurationPolicyState::Flattening,
                ),
                BandProtectionPolicy::Flatten {
                    trigger: BandFlattenTrigger::FlattenConfirm { .. },
                    ..
                } => (
                    curve_target,
                    current_exposure,
                    ConfigurationPolicyState::FlattenPending,
                ),
                BandProtectionPolicy::Terminate => (
                    curve_target,
                    Exposure(0.0),
                    ConfigurationPolicyState::Terminated,
                ),
            }
        }
    }
}

fn candidate_risk_outcome(
    candidate: &TrackDefinition,
    current_exposure: Exposure,
    policy_target: Exposure,
    loss_guard: LossGuardSnapshot,
) -> (Exposure, ConfigurationRiskOutcome) {
    let intent = ExposureIntent {
        current: current_exposure,
        target: policy_target,
        unit_notional: candidate.track_config().notional_per_unit,
        loss_guard,
    };
    match evaluate_risk_outcome(&intent, candidate.max_notional(), candidate.loss_limits()) {
        RiskOutcome::Allow { target } => (target, ConfigurationRiskOutcome::Allowed),
        RiskOutcome::Cap { target } => (target, ConfigurationRiskOutcome::Capped),
        RiskOutcome::Terminate(RiskTerminationCause::DailyLossLimit) => {
            (Exposure(0.0), ConfigurationRiskOutcome::DailyLossTerminated)
        }
        RiskOutcome::Terminate(RiskTerminationCause::TotalLossLimit) => {
            (Exposure(0.0), ConfigurationRiskOutcome::TotalLossTerminated)
        }
    }
}

fn quantity_for_exposure(exposure: Exposure, position_qty_per_unit: f64) -> PositionQty {
    position_qty(exposure.0 * position_qty_per_unit)
}

fn position_qty(value: f64) -> PositionQty {
    PositionQty::new(value).expect("preview quantities must be finite")
}

fn amount(value: f64, instrument_spec: &InstrumentSpec) -> Amount {
    Amount::new(value, instrument_spec.notional_asset().clone())
        .expect("preview notionals must be finite")
}

fn signed_notional(
    instrument_spec: &InstrumentSpec,
    quantity: PositionQty,
    strategy_price: f64,
) -> Amount {
    instrument_spec
        .signed_notional_from_position_qty(quantity, strategy_price)
        .expect("validated instrument spec and strategy price must convert signed notional")
}

fn unsigned_notional(
    instrument_spec: &InstrumentSpec,
    quantity: PositionQty,
    strategy_price: f64,
) -> Amount {
    instrument_spec
        .notional_from_position_qty(quantity, strategy_price)
        .expect("validated instrument spec and strategy price must convert notional")
}
