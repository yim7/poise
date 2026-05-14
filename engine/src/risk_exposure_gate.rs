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
    pub anchor_price: f64,
    pub anchor_curve_target: Exposure,
    #[serde(default = "default_anchor_started_at")]
    pub anchor_started_at: DateTime<Utc>,
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

    let previous_frontier = input
        .state
        .as_ref()
        .map(|state| state.risk_release_frontier.clone())
        .unwrap_or_else(|| input.current_exposure.clone());

    if crosses_zero(previous_frontier.0, input.curve_target.0) {
        return RiskExposureGateDecision {
            risk_release_frontier: Exposure(0.0),
            state: None,
            next_release: None,
        };
    }

    if inside_or_equal(input.curve_target.0, previous_frontier.0) {
        return RiskExposureGateDecision {
            risk_release_frontier: input.curve_target,
            state: None,
            next_release: None,
        };
    }

    let direction = if input.curve_target.0 > previous_frontier.0 {
        RiskIncreaseDirection::Long
    } else {
        RiskIncreaseDirection::Short
    };

    let mut state = input.state.unwrap_or_else(|| {
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
            return RiskExposureGateDecision {
                risk_release_frontier: input.curve_target,
                state: None,
                next_release: None,
            };
        }
    }

    let advantage_units = input.min_rebalance_units * config.advantage_steps;
    let reached_advantage = match direction {
        RiskIncreaseDirection::Long => {
            input.curve_target.0 >= state.anchor_curve_target.0 + advantage_units
        }
        RiskIncreaseDirection::Short => {
            input.curve_target.0 <= state.anchor_curve_target.0 - advantage_units
        }
    };

    let reached_stale_release = stale_release_due(config, &state, input.observed_at);
    if released_frontier_is_reached(
        direction,
        input.current_exposure.0,
        state.risk_release_frontier.0,
    ) && (reached_advantage || reached_stale_release)
    {
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
        state.anchor_price = input.strategy_price;
        state.anchor_curve_target = input.curve_target.clone();
        state.anchor_started_at = input.observed_at;
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
        anchor_price: strategy_price,
        anchor_curve_target: curve_target,
        anchor_started_at: observed_at,
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
        RiskIncreaseDirection::Long => Exposure(state.anchor_curve_target.0 + advantage_units),
        RiskIncreaseDirection::Short => Exposure(state.anchor_curve_target.0 - advantage_units),
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

fn stale_release_due(
    config: RiskAcquisitionConfig,
    state: &RiskExposureGateState,
    observed_at: DateTime<Utc>,
) -> bool {
    if config.stale_release_minutes <= f64::EPSILON {
        return false;
    }
    let elapsed_minutes = observed_at
        .signed_duration_since(state.anchor_started_at)
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

    if crosses_zero(current_exposure.0, desired_exposure.0) {
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

fn default_anchor_started_at() -> DateTime<Utc> {
    Utc::now()
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

    fn gate_state(frontier: f64, anchor_price: f64, anchor_curve: f64) -> RiskExposureGateState {
        RiskExposureGateState {
            risk_release_frontier: Exposure(frontier),
            anchor_price,
            anchor_curve_target: Exposure(anchor_curve),
            anchor_started_at: observed_at(),
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
                anchor_price: 100.0,
                anchor_curve_target: Exposure(5.0),
                anchor_started_at: observed_at(),
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
                anchor_price: 99.5,
                anchor_curve_target: Exposure(6.0),
                anchor_started_at: observed_at(),
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
                anchor_price: 99.8,
                anchor_curve_target: Exposure(5.5),
                anchor_started_at: later,
            })
        );
    }

    #[test]
    fn stale_wait_does_not_release_when_previous_release_is_unfilled() {
        let state = gate_state(1.5, 100.0, 5.0);
        let later = observed_at() + Duration::minutes(60);

        let decision = apply(input_at(Some(state.clone()), 0.75, 5.5, 99.8, later));

        assert_eq!(decision.risk_release_frontier, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
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
                anchor_price: 100.0,
                anchor_curve_target: Exposure(5.0),
                anchor_started_at: observed_at(),
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
                anchor_price: 100.0,
                anchor_curve_target: Exposure(-5.0),
                anchor_started_at: observed_at(),
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
    fn smaller_backlog_does_not_reduce_release_frontier() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state.clone()), 1.5, 4.0, 101.0));

        assert_eq!(decision.risk_release_frontier, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
    }

    #[test]
    fn curve_target_inside_release_frontier_reduces_immediately() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state), 1.5, 1.0, 102.0));

        assert_eq!(decision.risk_release_frontier, Exposure(1.0));
        assert_eq!(decision.state, None);
        assert_eq!(decision.next_release, None);
    }

    #[test]
    fn cross_zero_reduces_to_flat_before_new_direction() {
        let state = gate_state(1.5, 100.0, 5.0);

        let decision = apply(input(Some(state), 1.5, -1.0, 105.0));

        assert_eq!(decision.risk_release_frontier, Exposure(0.0));
        assert_eq!(decision.state, None);
        assert_eq!(decision.next_release, None);
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
    fn execution_target_flattens_before_reversing_direction() {
        assert_eq!(
            execution_target_exposure(&Exposure(3.0), &Exposure(-10.0), Some(&Exposure(-5.0))),
            Exposure(0.0)
        );
    }
}
