use chrono::{TimeZone, Utc};
use poise_core::instrument::{Amount, Asset, InstrumentSpec, PositionQty};
use poise_core::risk::{LossGuardSnapshot, LossLimits};
use poise_core::strategy::{
    BandFlattenTrigger, BandProtectionPolicy, BandRecoverPolicy, ShapeFamily, TrackConfig,
    desired_exposure,
};
use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
use poise_core::types::PricePrecision;
use poise_engine::configuration_preview::{
    ConfigurationPolicyState, ConfigurationPreviewBlocker, ConfigurationPreviewError,
    ConfigurationPreviewInput, ConfigurationRiskOutcome, preview_configuration,
};

fn definition(notional_per_unit: f64, max_notional: f64) -> TrackDefinition {
    TrackDefinition::try_new(
        TrackId::new("preview"),
        Instrument::new(Venue::Binance, "BTCUSDT"),
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        },
        Some(max_notional),
        loss_limits(),
        None,
    )
    .unwrap()
}

fn loss_limits() -> LossLimits {
    LossLimits {
        daily_loss_limit: 1_000.0,
        total_loss_limit: 2_000.0,
    }
}

fn linear_spec(step: f64, min_qty: f64, min_notional: f64) -> InstrumentSpec {
    let usdt = Asset::try_new("USDT").unwrap();
    InstrumentSpec::try_linear(
        Instrument::new(Venue::Binance, "BTCUSDT"),
        Asset::try_new("BTC").unwrap(),
        usdt.clone(),
        usdt.clone(),
        PricePrecision::FixedTick,
        0.1,
        step,
        min_qty,
        Amount::new(min_notional, usdt).unwrap(),
    )
    .unwrap()
}

fn inverse_spec(min_notional: f64) -> InstrumentSpec {
    let usd = Asset::try_new("USD").unwrap();
    InstrumentSpec::try_inverse(
        Instrument::new(Venue::Binance, "BTCUSDT"),
        Asset::try_new("BTC").unwrap(),
        usd.clone(),
        usd.clone(),
        Amount::new(100.0, usd.clone()).unwrap(),
        PricePrecision::FixedTick,
        0.1,
        1.0,
        1.0,
        Amount::new(min_notional, usd).unwrap(),
    )
    .unwrap()
}

fn input(
    active: TrackDefinition,
    candidate: TrackDefinition,
    spec: InstrumentSpec,
    position_qty: f64,
    price: f64,
    max_increase_notional: f64,
) -> ConfigurationPreviewInput {
    ConfigurationPreviewInput {
        active_policy_target: Some(desired_exposure(price, active.track_config())),
        active,
        candidate,
        instrument_spec: spec,
        current_position_qty: PositionQty::new(position_qty).unwrap(),
        loss_guard: LossGuardSnapshot {
            net_realized_pnl_today: 0.0,
            net_realized_pnl_cumulative: 0.0,
            unrealized_pnl: 0.0,
        },
        strategy_price: price,
        observed_at: Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap(),
        max_increase_notional,
    }
}

fn candidate_with_policy(policy: BandProtectionPolicy) -> TrackDefinition {
    let mut config = definition(100.0, 800.0).track_config().clone();
    config.out_of_band_policy = policy;
    TrackDefinition::try_new(
        TrackId::new("preview"),
        Instrument::new(Venue::Binance, "BTCUSDT"),
        config,
        Some(800.0),
        loss_limits(),
        None,
    )
    .unwrap()
}

#[test]
fn linear_preview_explains_targets_rebalance_capacity_and_fresh_frontier() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        definition(200.0, 1_600.0),
        linear_spec(0.1, 0.1, 10.0),
        2.0,
        95.0,
        1_500.0,
    ))
    .unwrap();

    assert_eq!(preview.active_current_exposure.0, 2.0);
    assert_eq!(preview.candidate_current_exposure.0, 1.0);
    assert_eq!(preview.active_policy_target.unwrap().0, 4.0);
    assert_eq!(preview.candidate_policy_target.0, 4.0);
    assert_eq!(
        preview.active_policy_target_position_qty.unwrap().value(),
        4.0
    );
    assert_eq!(preview.candidate_policy_target_position_qty.value(), 8.0);
    assert_eq!(preview.target_position_qty_delta.unwrap().value(), 4.0);
    assert_eq!(
        preview
            .active_policy_target_signed_notional
            .unwrap()
            .value(),
        380.0
    );
    assert_eq!(
        preview.candidate_policy_target_signed_notional.value(),
        760.0
    );
    assert_eq!(preview.target_signed_notional_delta.unwrap().value(), 380.0);
    assert_eq!(preview.candidate_target_delta_from_current_exposure.0, 3.0);
    assert_eq!(
        preview.candidate_position_qty_delta_from_current.value(),
        6.0
    );
    assert_eq!(preview.candidate_position_qty_per_unit, 2.0);
    assert_eq!(preview.minimum_rebalance_raw_quantity.value(), 1.0);
    assert_eq!(preview.minimum_rebalance_raw_notional.value(), 95.0);
    assert_eq!(preview.minimum_rebalance_quantity.value(), 1.0);
    assert_eq!(preview.minimum_rebalance_notional.value(), 95.0);
    assert_eq!(preview.maximum_target_position_qty.value(), 16.0);
    assert_eq!(preview.required_increase_notional, 1_410.0);
    assert_eq!(preview.available_increase_notional, 1_500.0);
    assert_eq!(preview.initial_frontier.0, 1.0);
    assert_eq!(preview.initial_execution_target.0, 1.0);
    assert!(preview.blockers.is_empty());
}

#[test]
fn inverse_preview_uses_contract_notional_without_multiplying_by_price() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        definition(100.0, 800.0),
        inverse_spec(100.0),
        3.0,
        50_000.0,
        500.0,
    ))
    .unwrap();

    assert_eq!(preview.candidate_position_qty_per_unit, 1.0);
    assert_eq!(preview.candidate_current_exposure.0, 3.0);
    assert_eq!(preview.minimum_rebalance_quantity.value(), 0.0);
    assert_eq!(preview.minimum_rebalance_notional.value(), 0.0);
    assert!(
        preview
            .blockers
            .contains(&ConfigurationPreviewBlocker::VenueMinimum)
    );
}

#[test]
fn changing_notional_per_unit_recalculates_exposure_from_unchanged_position_quantity() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        definition(200.0, 1_600.0),
        linear_spec(0.01, 0.01, 0.0),
        3.0,
        100.0,
        2_000.0,
    ))
    .unwrap();

    assert_eq!(preview.active_current_exposure.0, 3.0);
    assert_eq!(preview.candidate_current_exposure.0, 1.5);
}

#[test]
fn normalized_rebalance_must_meet_venue_minimum_after_step_rounding() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        definition(100.0, 800.0),
        linear_spec(0.6, 0.6, 60.0),
        0.0,
        100.0,
        800.0,
    ))
    .unwrap();

    assert_eq!(preview.minimum_rebalance_raw_quantity.value(), 0.5);
    assert_eq!(preview.minimum_rebalance_quantity.value(), 0.0);
    assert!(
        preview
            .blockers
            .contains(&ConfigurationPreviewBlocker::VenueMinimum)
    );
}

#[test]
fn insufficient_capacity_is_a_structured_blocker() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        definition(200.0, 1_600.0),
        linear_spec(0.1, 0.1, 0.0),
        1.0,
        100.0,
        500.0,
    ))
    .unwrap();

    assert_eq!(preview.required_increase_notional, 1_500.0);
    assert!(
        preview
            .blockers
            .contains(&ConfigurationPreviewBlocker::InsufficientIncreaseNotional)
    );
}

#[test]
fn unchanged_or_lower_risk_limits_do_not_require_new_capacity() {
    for candidate_max_notional in [800.0, 400.0] {
        let preview = preview_configuration(input(
            definition(100.0, 800.0),
            definition(100.0, candidate_max_notional),
            linear_spec(0.1, 0.1, 0.0),
            1.0,
            100.0,
            0.0,
        ))
        .unwrap();

        assert_eq!(preview.required_increase_notional, 0.0);
        assert!(
            !preview
                .blockers
                .contains(&ConfigurationPreviewBlocker::InsufficientIncreaseNotional)
        );
    }
}

#[test]
fn cross_zero_candidate_starts_from_a_flat_frontier() {
    let mut candidate = definition(100.0, 800.0);
    let mut config = candidate.track_config().clone();
    config.lower_price = 100.0;
    config.upper_price = 110.0;
    candidate = TrackDefinition::try_new(
        TrackId::new("preview"),
        Instrument::new(Venue::Binance, "BTCUSDT"),
        config,
        Some(800.0),
        loss_limits(),
        None,
    )
    .unwrap();
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        candidate,
        linear_spec(0.1, 0.1, 0.0),
        2.0,
        107.5,
        1_000.0,
    ))
    .unwrap();

    assert!(preview.candidate_policy_target.0 < 0.0);
    assert_eq!(preview.initial_frontier.0, 0.0);
    assert_eq!(preview.initial_execution_target.0, 0.0);
}

#[test]
fn non_finite_or_negative_public_inputs_are_rejected() {
    let mut invalid_price = input(
        definition(100.0, 800.0),
        definition(100.0, 800.0),
        linear_spec(0.1, 0.1, 0.0),
        0.0,
        100.0,
        0.0,
    );
    invalid_price.strategy_price = f64::NAN;
    assert_eq!(
        preview_configuration(invalid_price),
        Err(ConfigurationPreviewError::InvalidStrategyPrice)
    );

    let mut invalid_capacity = input(
        definition(100.0, 800.0),
        definition(100.0, 800.0),
        linear_spec(0.1, 0.1, 0.0),
        0.0,
        100.0,
        0.0,
    );
    invalid_capacity.max_increase_notional = -1.0;
    assert_eq!(
        preview_configuration(invalid_capacity),
        Err(ConfigurationPreviewError::InvalidMaxIncreaseNotional)
    );
}

#[test]
fn freeze_out_of_band_keeps_current_exposure_as_candidate_policy_target() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        candidate_with_policy(BandProtectionPolicy::Freeze),
        linear_spec(0.1, 0.1, 0.0),
        2.0,
        85.0,
        0.0,
    ))
    .unwrap();

    assert_eq!(preview.candidate_curve_target.0, 8.0);
    assert_eq!(preview.candidate_policy_target.0, 2.0);
    assert_eq!(
        preview.candidate_policy_state,
        ConfigurationPolicyState::Frozen
    );
    assert_eq!(preview.initial_frontier.0, 2.0);
}

#[test]
fn immediate_flatten_out_of_band_targets_flat_and_marks_flattening() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        candidate_with_policy(BandProtectionPolicy::Flatten {
            trigger: BandFlattenTrigger::Immediate,
            recover: BandRecoverPolicy::BackInBand,
        }),
        linear_spec(0.1, 0.1, 0.0),
        2.0,
        85.0,
        0.0,
    ))
    .unwrap();

    assert_eq!(preview.candidate_policy_target.0, 0.0);
    assert_eq!(
        preview.candidate_policy_state,
        ConfigurationPolicyState::Flattening
    );
    assert_eq!(preview.initial_execution_target.0, 0.0);
}

#[test]
fn confirmed_flatten_out_of_band_keeps_current_target_and_marks_pending() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        candidate_with_policy(BandProtectionPolicy::Flatten {
            trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
            recover: BandRecoverPolicy::BackInBand,
        }),
        linear_spec(0.1, 0.1, 0.0),
        2.0,
        85.0,
        0.0,
    ))
    .unwrap();

    assert_eq!(preview.candidate_policy_target.0, 2.0);
    assert_eq!(
        preview.candidate_policy_state,
        ConfigurationPolicyState::FlattenPending
    );
}

#[test]
fn terminate_out_of_band_targets_flat_and_marks_terminated() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        candidate_with_policy(BandProtectionPolicy::Terminate),
        linear_spec(0.1, 0.1, 0.0),
        2.0,
        85.0,
        0.0,
    ))
    .unwrap();

    assert_eq!(preview.candidate_policy_target.0, 0.0);
    assert_eq!(
        preview.candidate_policy_state,
        ConfigurationPolicyState::Terminated
    );
}

#[test]
fn absent_active_decision_target_keeps_cross_configuration_comparisons_unknown() {
    let mut preview_input = input(
        definition(100.0, 800.0),
        definition(100.0, 800.0),
        linear_spec(0.1, 0.1, 0.0),
        1.0,
        95.0,
        0.0,
    );
    preview_input.active_policy_target = None;

    let preview = preview_configuration(preview_input).unwrap();

    assert_eq!(preview.active_policy_target, None);
    assert_eq!(preview.active_policy_target_position_qty, None);
    assert_eq!(preview.active_policy_target_signed_notional, None);
    assert_eq!(preview.target_position_qty_delta, None);
    assert_eq!(preview.target_signed_notional_delta, None);
}

#[test]
fn candidate_max_notional_caps_the_target_before_initializing_the_fresh_frontier() {
    let preview = preview_configuration(input(
        definition(100.0, 800.0),
        definition(100.0, 200.0),
        linear_spec(0.1, 0.1, 0.0),
        0.0,
        95.0,
        0.0,
    ))
    .unwrap();

    assert_eq!(preview.candidate_policy_target.0, 4.0);
    assert_eq!(preview.candidate_risk_approved_target.0, 2.0);
    assert_eq!(
        preview.candidate_risk_outcome,
        ConfigurationRiskOutcome::Capped
    );
    assert_eq!(preview.initial_frontier.0, 0.0);
    assert_eq!(preview.initial_execution_target.0, 0.0);
}

#[test]
fn lower_candidate_loss_limit_terminates_the_candidate_target_before_gate_initialization() {
    let candidate_config = definition(100.0, 800.0).track_config().clone();
    let candidate = TrackDefinition::try_new(
        TrackId::new("preview"),
        Instrument::new(Venue::Binance, "BTCUSDT"),
        candidate_config,
        Some(800.0),
        LossLimits {
            daily_loss_limit: 10.0,
            total_loss_limit: 100.0,
        },
        None,
    )
    .unwrap();
    let mut preview_input = input(
        definition(100.0, 800.0),
        candidate,
        linear_spec(0.1, 0.1, 0.0),
        2.0,
        95.0,
        0.0,
    );
    preview_input.loss_guard.net_realized_pnl_today = -11.0;

    let preview = preview_configuration(preview_input).unwrap();

    assert_eq!(preview.candidate_risk_approved_target.0, 0.0);
    assert_eq!(
        preview.candidate_risk_outcome,
        ConfigurationRiskOutcome::DailyLossTerminated
    );
    assert_eq!(preview.initial_execution_target.0, 0.0);
}
