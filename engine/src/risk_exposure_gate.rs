use chrono::{DateTime, Utc};
use poise_core::strategy::RiskAcquisitionConfig;
use poise_core::types::Exposure;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskIncreaseDirection {
    Long,
    Short,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskExposureGateState {
    pub risk_release_frontier: Exposure,
    pub release_anchor_price: f64,
    pub release_anchor_target: Exposure,
    pub stale_since: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskAcquisitionRelease {
    pub direction: RiskIncreaseDirection,
    pub release_target: Exposure,
    pub release_units: f64,
    pub advantage_target: Exposure,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskExposureGateInput {
    pub config: RiskAcquisitionConfig,
    pub min_rebalance_units: f64,
    pub state: Option<RiskExposureGateState>,
    pub current_exposure: Exposure,
    pub curve_target: Exposure,
    pub strategy_price: f64,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskExposureGateDecision {
    pub risk_release_frontier: Exposure,
    pub state: Option<RiskExposureGateState>,
    pub next_release: Option<RiskAcquisitionRelease>,
}

pub fn apply(input: RiskExposureGateInput) -> RiskExposureGateDecision {
    let config = input.config;

    let mut gate_state = input.state.clone();
    let mut previous_frontier = gate_state
        .as_ref()
        .map(|state| state.risk_release_frontier.clone())
        .unwrap_or_else(|| input.current_exposure.clone());

    if crosses_zero(previous_frontier.0, input.curve_target.0) {
        previous_frontier = Exposure(0.0);
        gate_state = Some(zero_anchor_state(input.strategy_price, input.observed_at));
    }

    if inside_or_equal(input.curve_target.0, previous_frontier.0) {
        let risk_release_frontier = input.curve_target.clone();
        return clamp_released_frontier(
            gate_state,
            risk_release_frontier,
            input.strategy_price,
            input.observed_at,
        );
    }

    let direction = if input.curve_target.0 > previous_frontier.0 {
        RiskIncreaseDirection::Long
    } else {
        RiskIncreaseDirection::Short
    };

    let mut state = gate_state.unwrap_or_else(|| {
        startup_state(
            config,
            input.min_rebalance_units,
            input.current_exposure.clone(),
            input.curve_target.clone(),
            input.strategy_price,
            input.observed_at,
        )
    });

    if !same_direction_backlog(
        direction,
        state.risk_release_frontier.0,
        input.curve_target.0,
    ) {
        state = startup_state(
            config,
            input.min_rebalance_units,
            input.current_exposure.clone(),
            input.curve_target.clone(),
            input.strategy_price,
            input.observed_at,
        );
    }

    if released_frontier_is_reached(
        direction,
        input.current_exposure.0,
        state.risk_release_frontier.0,
    ) && !current_exceeds_desired(direction, input.current_exposure.0, input.curve_target.0)
    {
        state.risk_release_frontier = ratchet_frontier(
            direction,
            state.risk_release_frontier,
            input.current_exposure.clone(),
            input.curve_target.clone(),
        );
        if inside_or_equal(input.curve_target.0, state.risk_release_frontier.0) {
            return clamp_state_to_frontier(
                state,
                input.curve_target,
                input.strategy_price,
                input.observed_at,
            );
        }
    }

    let advantage_units = input.min_rebalance_units * config.advantage_steps;
    let reached_advantage = match direction {
        RiskIncreaseDirection::Long => {
            input.curve_target.0 >= state.release_anchor_target.0 + advantage_units
        }
        RiskIncreaseDirection::Short => {
            input.curve_target.0 <= state.release_anchor_target.0 - advantage_units
        }
    };

    let reached_stale_release = stale_release_due(config, &state, input.observed_at);
    if reached_advantage || reached_stale_release {
        let release_units = release_units(
            config,
            input.min_rebalance_units,
            state.risk_release_frontier.0,
            input.curve_target.0,
        );
        state.risk_release_frontier = move_toward(
            state.risk_release_frontier,
            input.curve_target.clone(),
            release_units,
        );
        if reached_advantage {
            state.release_anchor_price = input.strategy_price;
            state.release_anchor_target = input.curve_target.clone();
        }
        state.stale_since = input.observed_at;
    }

    let next_release = next_release(
        config,
        input.min_rebalance_units,
        &state,
        input.curve_target,
    );

    RiskExposureGateDecision {
        risk_release_frontier: state.risk_release_frontier.clone(),
        state: Some(state),
        next_release,
    }
}

fn clamp_released_frontier(
    state: Option<RiskExposureGateState>,
    risk_release_frontier: Exposure,
    strategy_price: f64,
    observed_at: DateTime<Utc>,
) -> RiskExposureGateDecision {
    if let Some(state) = state {
        return clamp_state_to_frontier(state, risk_release_frontier, strategy_price, observed_at);
    }
    RiskExposureGateDecision {
        risk_release_frontier,
        state: None,
        next_release: None,
    }
}

fn zero_anchor_state(strategy_price: f64, observed_at: DateTime<Utc>) -> RiskExposureGateState {
    RiskExposureGateState {
        risk_release_frontier: Exposure(0.0),
        release_anchor_price: strategy_price,
        release_anchor_target: Exposure(0.0),
        stale_since: observed_at,
    }
}

fn clamp_state_to_frontier(
    mut state: RiskExposureGateState,
    risk_release_frontier: Exposure,
    strategy_price: f64,
    observed_at: DateTime<Utc>,
) -> RiskExposureGateDecision {
    state.risk_release_frontier = risk_release_frontier.clone();
    state.release_anchor_price = strategy_price;
    state.release_anchor_target = risk_release_frontier.clone();
    state.stale_since = observed_at;
    RiskExposureGateDecision {
        risk_release_frontier,
        state: Some(state),
        next_release: None,
    }
}

fn startup_state(
    config: RiskAcquisitionConfig,
    min_rebalance_units: f64,
    current_exposure: Exposure,
    curve_target: Exposure,
    strategy_price: f64,
    observed_at: DateTime<Utc>,
) -> RiskExposureGateState {
    let target_units = curve_target.0.abs();
    let ratio_units = target_units * config.initial_ratio;
    let initial_units = if target_units < min_rebalance_units {
        target_units
    } else {
        ratio_units.max(min_rebalance_units).min(target_units)
    };
    let current_units = if current_exposure.0.signum() == curve_target.0.signum() {
        current_exposure.0.abs().min(target_units)
    } else {
        0.0
    };
    let frontier_units = initial_units.max(current_units).min(target_units);
    RiskExposureGateState {
        risk_release_frontier: Exposure(curve_target.0.signum() * frontier_units),
        release_anchor_price: strategy_price,
        release_anchor_target: curve_target,
        stale_since: observed_at,
    }
}

fn release_units(
    config: RiskAcquisitionConfig,
    min_rebalance_units: f64,
    frontier: f64,
    curve: f64,
) -> f64 {
    let backlog_units = (curve - frontier).abs();
    let base_step_units = min_rebalance_units * config.min_release_steps;
    let max_step_units = min_rebalance_units * config.max_release_steps;
    let dynamic_units = backlog_units * config.catchup_ratio;
    dynamic_units
        .clamp(base_step_units, max_step_units)
        .min(backlog_units)
}

fn next_release(
    config: RiskAcquisitionConfig,
    min_rebalance_units: f64,
    state: &RiskExposureGateState,
    curve_target: Exposure,
) -> Option<RiskAcquisitionRelease> {
    let direction = if curve_target.0 > state.risk_release_frontier.0 {
        RiskIncreaseDirection::Long
    } else if curve_target.0 < state.risk_release_frontier.0 {
        RiskIncreaseDirection::Short
    } else {
        return None;
    };
    let release_units = release_units(
        config,
        min_rebalance_units,
        state.risk_release_frontier.0,
        curve_target.0,
    );
    if release_units <= f64::EPSILON {
        return None;
    }
    let advantage_units = min_rebalance_units * config.advantage_steps;
    let advantage_target = match direction {
        RiskIncreaseDirection::Long => Exposure(state.release_anchor_target.0 + advantage_units),
        RiskIncreaseDirection::Short => Exposure(state.release_anchor_target.0 - advantage_units),
    };
    Some(RiskAcquisitionRelease {
        direction,
        release_target: move_toward(
            state.risk_release_frontier.clone(),
            curve_target,
            release_units,
        ),
        release_units,
        advantage_target,
    })
}

pub fn pending_release(
    config: RiskAcquisitionConfig,
    min_rebalance_units: f64,
    state: &RiskExposureGateState,
    curve_target: Exposure,
) -> Option<RiskAcquisitionRelease> {
    next_release(config, min_rebalance_units, state, curve_target)
}

fn stale_release_due(
    config: RiskAcquisitionConfig,
    state: &RiskExposureGateState,
    observed_at: DateTime<Utc>,
) -> bool {
    if config.stale_release_minutes <= f64::EPSILON {
        return false;
    }
    let elapsed_minutes = observed_at
        .signed_duration_since(state.stale_since)
        .num_milliseconds() as f64
        / 60_000.0;
    elapsed_minutes + f64::EPSILON >= config.stale_release_minutes
}

fn released_frontier_is_reached(
    direction: RiskIncreaseDirection,
    current_exposure: f64,
    risk_release_frontier: f64,
) -> bool {
    match direction {
        RiskIncreaseDirection::Long => current_exposure + f64::EPSILON >= risk_release_frontier,
        RiskIncreaseDirection::Short => current_exposure - f64::EPSILON <= risk_release_frontier,
    }
}

fn current_exceeds_desired(
    direction: RiskIncreaseDirection,
    current_exposure: f64,
    desired_exposure: f64,
) -> bool {
    match direction {
        RiskIncreaseDirection::Long => current_exposure > desired_exposure + f64::EPSILON,
        RiskIncreaseDirection::Short => current_exposure < desired_exposure - f64::EPSILON,
    }
}

fn ratchet_frontier(
    direction: RiskIncreaseDirection,
    risk_release_frontier: Exposure,
    current_exposure: Exposure,
    desired_exposure: Exposure,
) -> Exposure {
    match direction {
        RiskIncreaseDirection::Long => Exposure(
            current_exposure
                .0
                .max(risk_release_frontier.0)
                .min(desired_exposure.0),
        ),
        RiskIncreaseDirection::Short => Exposure(
            current_exposure
                .0
                .min(risk_release_frontier.0)
                .max(desired_exposure.0),
        ),
    }
}

pub fn execution_target_exposure(
    current_exposure: &Exposure,
    desired_exposure: &Exposure,
    risk_release_frontier: Option<&Exposure>,
) -> Exposure {
    let Some(frontier) = risk_release_frontier else {
        return desired_exposure.clone();
    };

    if crosses_zero(frontier.0, desired_exposure.0) {
        return Exposure(0.0);
    }
    if inside_or_equal(desired_exposure.0, frontier.0) {
        return desired_exposure.clone();
    }

    if desired_exposure.0 > frontier.0 {
        if current_exposure.0 < frontier.0 {
            frontier.clone()
        } else if current_exposure.0 <= desired_exposure.0 {
            current_exposure.clone()
        } else {
            desired_exposure.clone()
        }
    } else if desired_exposure.0 < frontier.0 {
        if current_exposure.0 > frontier.0 {
            frontier.clone()
        } else if current_exposure.0 >= desired_exposure.0 {
            current_exposure.clone()
        } else {
            desired_exposure.clone()
        }
    } else {
        desired_exposure.clone()
    }
}

fn move_toward(from: Exposure, to: Exposure, units: f64) -> Exposure {
    if to.0 > from.0 {
        Exposure((from.0 + units).min(to.0))
    } else {
        Exposure((from.0 - units).max(to.0))
    }
}

fn crosses_zero(frontier: f64, curve_target: f64) -> bool {
    (frontier > f64::EPSILON && curve_target < -f64::EPSILON)
        || (frontier < -f64::EPSILON && curve_target > f64::EPSILON)
}

fn inside_or_equal(curve_target: f64, frontier: f64) -> bool {
    if frontier > f64::EPSILON {
        curve_target <= frontier
    } else if frontier < -f64::EPSILON {
        curve_target >= frontier
    } else {
        curve_target.abs() <= f64::EPSILON
    }
}

fn same_direction_backlog(
    direction: RiskIncreaseDirection,
    frontier: f64,
    curve_target: f64,
) -> bool {
    match direction {
        RiskIncreaseDirection::Long => curve_target > frontier,
        RiskIncreaseDirection::Short => curve_target < frontier,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use poise_core::strategy::RiskAcquisitionConfig;
    use poise_core::types::Exposure;

    use super::*;

    fn config() -> RiskAcquisitionConfig {
        RiskAcquisitionConfig {
            initial_ratio: 0.5,
            advantage_steps: 2.0,
            min_release_steps: 1.0,
            max_release_steps: 4.0,
            catchup_ratio: 0.25,
            stale_release_minutes: 60.0,
        }
    }

    fn observed_at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 11, 9, 0, 0).unwrap()
    }

    fn gate_state(
        frontier: f64,
        release_anchor_price: f64,
        release_anchor: f64,
    ) -> RiskExposureGateState {
        RiskExposureGateState {
            risk_release_frontier: Exposure(frontier),
            release_anchor_price,
            release_anchor_target: Exposure(release_anchor),
            stale_since: observed_at(),
        }
    }

    fn input(
        state: Option<RiskExposureGateState>,
        current_exposure: f64,
        curve_target: f64,
        price: f64,
    ) -> RiskExposureGateInput {
        RiskExposureGateInput {
            config: config(),
            min_rebalance_units: 0.5,
            state,
            current_exposure: Exposure(current_exposure),
            curve_target: Exposure(curve_target),
            strategy_price: price,
            observed_at: observed_at(),
        }
    }

    fn input_at(
        state: Option<RiskExposureGateState>,
        current_exposure: f64,
        curve_target: f64,
        price: f64,
        observed_at: DateTime<Utc>,
    ) -> RiskExposureGateInput {
        RiskExposureGateInput {
            observed_at,
            ..input(state, current_exposure, curve_target, price)
        }
    }

    #[test]
    fn startup_releases_initial_ratio_and_keeps_backlog() {
        let decision = apply(input(None, 0.0, 5.0, 100.0));

        assert_eq!(decision.risk_release_frontier, Exposure(2.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(2.5),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(5.0),
                stale_since: observed_at(),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn cross_zero_opposite_residual_starts_from_zero_without_initial_release() {
        let decision = apply(input(None, -0.25, 0.75, 100.0));

        assert_eq!(decision.risk_release_frontier, Exposure(0.0));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(0.0),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(0.0),
                stale_since: observed_at(),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn cross_zero_opposite_residual_releases_after_advantage_from_zero() {
        let decision = apply(input(None, -0.25, 1.0, 100.0));

        assert_eq!(decision.risk_release_frontier, Exposure(0.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(0.5),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(1.0),
                stale_since: observed_at(),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn does_not_release_before_advantage_target() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state.clone()), 1.5, 5.9, 99.6));

        assert_eq!(decision.risk_release_frontier, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
    }

    #[test]
    fn releases_dynamic_step_after_advantage() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state), 1.5, 6.0, 99.5));

        assert_eq!(decision.risk_release_frontier, Exposure(2.625));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(2.625),
                release_anchor_price: 99.5,
                release_anchor_target: Exposure(6.0),
                stale_since: observed_at(),
            })
        );
    }

    #[test]
    fn releases_dynamic_step_after_stale_wait_without_price_advantage() {
        let state = gate_state(1.5, 100.0, 5.0);
        let later = observed_at() + Duration::minutes(60);

        let decision = apply(input_at(Some(state), 1.5, 5.5, 99.8, later));

        assert_eq!(decision.risk_release_frontier, Exposure(2.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(2.5),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(5.0),
                stale_since: later,
            })
        );
    }

    #[test]
    fn stale_wait_releases_even_when_previous_release_is_unfilled() {
        let state = gate_state(1.5, 100.0, 5.0);
        let later = observed_at() + Duration::minutes(60);

        let decision = apply(input_at(Some(state), 0.75, 5.5, 99.8, later));

        assert_eq!(decision.risk_release_frontier, Exposure(2.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(2.5),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(5.0),
                stale_since: later,
            })
        );
    }

    #[test]
    fn stale_wait_repeats_without_position_progress() {
        let state = gate_state(1.5, 100.0, 5.0);
        let first_release_at = observed_at() + Duration::minutes(60);
        let first = apply(input_at(Some(state), 0.75, 5.5, 99.8, first_release_at));
        let second_release_at = first_release_at + Duration::minutes(60);

        let decision = apply(input_at(
            first.state.clone(),
            0.75,
            5.8,
            99.7,
            second_release_at,
        ));

        assert_eq!(decision.risk_release_frontier, Exposure(3.325));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(3.325),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(5.0),
                stale_since: second_release_at,
            })
        );
    }

    #[test]
    fn stale_wait_uses_same_clock_after_position_reaches_release_frontier() {
        let state = gate_state(1.5, 100.0, 5.0);
        let first_release_at = observed_at() + Duration::minutes(60);
        let first = apply(input_at(Some(state), 0.75, 5.5, 99.8, first_release_at));
        let second_release_at = first_release_at + Duration::minutes(60);

        let decision = apply(input_at(first.state, 2.5, 5.8, 99.7, second_release_at));

        assert_eq!(decision.risk_release_frontier, Exposure(3.325));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(3.325),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(5.0),
                stale_since: second_release_at,
            })
        );
    }

    #[test]
    fn advantage_releases_even_when_previous_release_is_unfilled() {
        let state = gate_state(-1.5, 100.0, -5.0);
        let advantage_at = observed_at() + Duration::minutes(14);

        let decision = apply(input_at(Some(state), -0.75, -6.0, 100.8, advantage_at));

        assert_eq!(decision.risk_release_frontier, Exposure(-2.625));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(-2.625),
                release_anchor_price: 100.8,
                release_anchor_target: Exposure(-6.0),
                stale_since: advantage_at,
            })
        );
    }

    #[test]
    fn advantage_release_resets_stale_wait_clock() {
        let state = gate_state(-1.5, 100.0, -5.0);
        let advantage_at = observed_at() + Duration::minutes(14);
        let first = apply(input_at(Some(state), -0.75, -6.0, 100.8, advantage_at));
        let before_stale_from_advantage = advantage_at + Duration::minutes(59);

        let decision = apply(input_at(
            first.state.clone(),
            -0.75,
            -6.2,
            100.9,
            before_stale_from_advantage,
        ));

        assert_eq!(decision.risk_release_frontier, Exposure(-2.625));
        assert_eq!(decision.state, first.state);
    }

    #[test]
    fn current_past_frontier_ratchets_without_releasing_or_resetting_anchor() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state), 2.0, 5.5, 99.8));

        assert_eq!(decision.risk_release_frontier, Exposure(2.0));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(2.0),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(5.0),
                stale_since: observed_at(),
            })
        );
    }

    #[test]
    fn short_current_past_frontier_ratchets_without_releasing() {
        let state = gate_state(-1.5, 100.0, -5.0);

        let decision = apply(input(Some(state), -2.0, -5.5, 100.2));

        assert_eq!(decision.risk_release_frontier, Exposure(-2.0));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(-2.0),
                release_anchor_price: 100.0,
                release_anchor_target: Exposure(-5.0),
                stale_since: observed_at(),
            })
        );
    }

    #[test]
    fn zero_stale_release_minutes_disables_time_release() {
        let state = gate_state(1.5, 100.0, 5.0);
        let later = observed_at() + Duration::minutes(60);
        let mut input = input_at(Some(state.clone()), 1.5, 5.5, 99.8, later);
        input.config.stale_release_minutes = 0.0;

        let decision = apply(input);

        assert_eq!(decision.risk_release_frontier, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
    }

    #[test]
    fn curve_pullback_outside_current_keeps_released_frontier() {
        let state = gate_state(2.625, 99.5, 6.0);
        let pullback_at = observed_at() + Duration::minutes(15);

        let decision = apply(input_at(Some(state), 2.625, 5.0, 100.0, pullback_at));

        assert_eq!(decision.risk_release_frontier, Exposure(2.625));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(2.625),
                release_anchor_price: 99.5,
                release_anchor_target: Exposure(6.0),
                stale_since: observed_at(),
            })
        );
    }

    #[test]
    fn smaller_backlog_does_not_reduce_release_frontier() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state.clone()), 1.5, 4.0, 101.0));

        assert_eq!(decision.risk_release_frontier, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
    }

    #[test]
    fn short_curve_pullback_outside_current_keeps_released_frontier() {
        let state = gate_state(-2.625, 100.8, -6.0);
        let pullback_at = observed_at() + Duration::minutes(15);

        let decision = apply(input_at(Some(state), -2.625, -5.0, 100.0, pullback_at));

        assert_eq!(decision.risk_release_frontier, Exposure(-2.625));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(-2.625),
                release_anchor_price: 100.8,
                release_anchor_target: Exposure(-6.0),
                stale_since: observed_at(),
            })
        );
    }

    #[test]
    fn curve_target_inside_release_frontier_reduces_immediately() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state), 1.5, 1.0, 102.0));

        assert_eq!(decision.risk_release_frontier, Exposure(1.0));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(1.0),
                release_anchor_price: 102.0,
                release_anchor_target: Exposure(1.0),
                stale_since: observed_at(),
            })
        );
        assert_eq!(decision.next_release, None);
    }

    #[test]
    fn cross_zero_projects_frontier_to_zero_and_waits_for_release_trigger() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state), 1.5, -0.5, 105.0));

        assert_eq!(decision.risk_release_frontier, Exposure(0.0));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(0.0),
                release_anchor_price: 105.0,
                release_anchor_target: Exposure(0.0),
                stale_since: observed_at(),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn cross_zero_projected_frontier_releases_after_stale_wait() {
        let state = gate_state(1.5, 100.0, 5.0);
        let flat = apply(input(Some(state), 1.5, -0.5, 105.0));
        let later = observed_at() + Duration::minutes(60);

        let decision = apply(input_at(flat.state, 0.0, -0.75, 105.2, later));

        assert_eq!(decision.risk_release_frontier, Exposure(-0.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(-0.5),
                release_anchor_price: 105.0,
                release_anchor_target: Exposure(0.0),
                stale_since: later,
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn cross_zero_projected_frontier_releases_immediately_when_advantage_is_reached() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state), 1.5, -1.0, 106.0));

        assert_eq!(decision.risk_release_frontier, Exposure(-0.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(-0.5),
                release_anchor_price: 106.0,
                release_anchor_target: Exposure(-1.0),
                stale_since: observed_at(),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn cross_zero_frontier_releases_after_advantage_without_waiting_for_flat_position() {
        let state = gate_state(0.0, 105.0, 0.0);

        let decision = apply(input(Some(state), 0.25, -1.0, 106.0));

        assert_eq!(decision.risk_release_frontier, Exposure(-0.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(-0.5),
                release_anchor_price: 106.0,
                release_anchor_target: Exposure(-1.0),
                stale_since: observed_at(),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn frontier_clamped_to_zero_does_not_grant_initial_release_when_desired_grows() {
        let state = gate_state(1.5, 100.0, 5.0);
        let flat = apply(input(Some(state), 1.5, 0.0, 105.0));

        let decision = apply(input(flat.state, 0.0, 0.5, 104.8));

        assert_eq!(decision.risk_release_frontier, Exposure(0.0));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                risk_release_frontier: Exposure(0.0),
                release_anchor_price: 105.0,
                release_anchor_target: Exposure(0.0),
                stale_since: observed_at(),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn execution_target_uses_frontier_for_unreached_long_release() {
        assert_eq!(
            execution_target_exposure(&Exposure(0.0), &Exposure(10.0), Some(&Exposure(5.0))),
            Exposure(5.0)
        );
    }

    #[test]
    fn execution_target_holds_when_long_current_is_between_frontier_and_desired() {
        assert_eq!(
            execution_target_exposure(&Exposure(6.0), &Exposure(10.0), Some(&Exposure(5.0))),
            Exposure(6.0)
        );
    }

    #[test]
    fn execution_target_reduces_when_long_current_exceeds_desired() {
        assert_eq!(
            execution_target_exposure(&Exposure(12.0), &Exposure(10.0), Some(&Exposure(5.0))),
            Exposure(10.0)
        );
    }

    #[test]
    fn execution_target_uses_frontier_for_unreached_short_release() {
        assert_eq!(
            execution_target_exposure(&Exposure(0.0), &Exposure(-10.0), Some(&Exposure(-5.0))),
            Exposure(-5.0)
        );
    }

    #[test]
    fn execution_target_holds_when_short_current_is_between_frontier_and_desired() {
        assert_eq!(
            execution_target_exposure(&Exposure(-6.0), &Exposure(-10.0), Some(&Exposure(-5.0))),
            Exposure(-6.0)
        );
    }

    #[test]
    fn execution_target_reduces_when_short_current_exceeds_desired() {
        assert_eq!(
            execution_target_exposure(&Exposure(-12.0), &Exposure(-10.0), Some(&Exposure(-5.0))),
            Exposure(-10.0)
        );
    }

    #[test]
    fn execution_target_uses_released_frontier_when_current_is_opposite_direction_residual() {
        assert_eq!(
            execution_target_exposure(&Exposure(3.0), &Exposure(-10.0), Some(&Exposure(-5.0))),
            Exposure(-5.0)
        );
    }

    #[test]
    fn execution_target_flattens_when_frontier_has_not_crossed_zero() {
        assert_eq!(
            execution_target_exposure(&Exposure(3.0), &Exposure(-10.0), Some(&Exposure(1.0))),
            Exposure(0.0)
        );
    }
}
