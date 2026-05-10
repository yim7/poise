use poise_core::strategy::RiskIncreaseDelayConfig;
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
    pub allowed_target: Exposure,
    pub anchor_price: f64,
    pub anchor_curve_target: Exposure,
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
    pub config: Option<RiskIncreaseDelayConfig>,
    pub min_rebalance_units: f64,
    pub state: Option<RiskExposureGateState>,
    pub current_exposure: Exposure,
    pub curve_target: Exposure,
    pub strategy_price: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskExposureGateDecision {
    pub allowed_target: Exposure,
    pub state: Option<RiskExposureGateState>,
    pub next_release: Option<RiskAcquisitionRelease>,
}

pub fn apply(input: RiskExposureGateInput) -> RiskExposureGateDecision {
    let Some(config) = input.config else {
        return follow_curve(input.curve_target);
    };

    let previous_allowed = input
        .state
        .as_ref()
        .map(|state| state.allowed_target.clone())
        .unwrap_or_else(|| input.current_exposure.clone());

    if crosses_zero(previous_allowed.0, input.curve_target.0) {
        return RiskExposureGateDecision {
            allowed_target: Exposure(0.0),
            state: None,
            next_release: None,
        };
    }

    if inside_or_equal(input.curve_target.0, previous_allowed.0) {
        return RiskExposureGateDecision {
            allowed_target: input.curve_target,
            state: None,
            next_release: None,
        };
    }

    let direction = if input.curve_target.0 > previous_allowed.0 {
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
        )
    });

    if !same_direction_backlog(direction, state.allowed_target.0, input.curve_target.0) {
        state = startup_state(
            config,
            input.min_rebalance_units,
            input.current_exposure.clone(),
            input.curve_target.clone(),
            input.strategy_price,
        );
    }

    let advantage_units = input.min_rebalance_units * config.advantage_min_rebalance_multiples;
    let reached_advantage = match direction {
        RiskIncreaseDirection::Long => {
            input.curve_target.0 >= state.anchor_curve_target.0 + advantage_units
        }
        RiskIncreaseDirection::Short => {
            input.curve_target.0 <= state.anchor_curve_target.0 - advantage_units
        }
    };

    if reached_advantage {
        let release_units = release_units(
            config,
            input.min_rebalance_units,
            state.allowed_target.0,
            input.curve_target.0,
        );
        state.allowed_target = move_toward(
            state.allowed_target,
            input.curve_target.clone(),
            release_units,
        );
        state.anchor_price = input.strategy_price;
        state.anchor_curve_target = input.curve_target.clone();
    }

    let next_release = next_release(
        config,
        input.min_rebalance_units,
        &state,
        input.curve_target,
    );

    RiskExposureGateDecision {
        allowed_target: state.allowed_target.clone(),
        state: Some(state),
        next_release,
    }
}

fn follow_curve(curve_target: Exposure) -> RiskExposureGateDecision {
    RiskExposureGateDecision {
        allowed_target: curve_target,
        state: None,
        next_release: None,
    }
}

fn startup_state(
    config: RiskIncreaseDelayConfig,
    min_rebalance_units: f64,
    current_exposure: Exposure,
    curve_target: Exposure,
    strategy_price: f64,
) -> RiskExposureGateState {
    let target_units = curve_target.0.abs();
    let ratio_units = target_units * config.startup_initial_ratio;
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
    let allowed_units = initial_units.max(current_units).min(target_units);
    RiskExposureGateState {
        allowed_target: Exposure(curve_target.0.signum() * allowed_units),
        anchor_price: strategy_price,
        anchor_curve_target: curve_target,
    }
}

fn release_units(
    config: RiskIncreaseDelayConfig,
    min_rebalance_units: f64,
    allowed: f64,
    curve: f64,
) -> f64 {
    let backlog_units = (curve - allowed).abs();
    let base_step_units = min_rebalance_units * config.base_step_min_rebalance_multiples;
    let max_step_units = min_rebalance_units * config.max_step_min_rebalance_multiples;
    let dynamic_units = backlog_units * config.catchup_ratio;
    dynamic_units
        .clamp(base_step_units, max_step_units)
        .min(backlog_units)
}

fn next_release(
    config: RiskIncreaseDelayConfig,
    min_rebalance_units: f64,
    state: &RiskExposureGateState,
    curve_target: Exposure,
) -> Option<RiskAcquisitionRelease> {
    let direction = if curve_target.0 > state.allowed_target.0 {
        RiskIncreaseDirection::Long
    } else if curve_target.0 < state.allowed_target.0 {
        RiskIncreaseDirection::Short
    } else {
        return None;
    };
    let release_units = release_units(
        config,
        min_rebalance_units,
        state.allowed_target.0,
        curve_target.0,
    );
    if release_units <= f64::EPSILON {
        return None;
    }
    let advantage_units = min_rebalance_units * config.advantage_min_rebalance_multiples;
    let advantage_target = match direction {
        RiskIncreaseDirection::Long => Exposure(state.anchor_curve_target.0 + advantage_units),
        RiskIncreaseDirection::Short => Exposure(state.anchor_curve_target.0 - advantage_units),
    };
    Some(RiskAcquisitionRelease {
        direction,
        release_target: move_toward(state.allowed_target.clone(), curve_target, release_units),
        release_units,
        advantage_target,
    })
}

fn move_toward(from: Exposure, to: Exposure, units: f64) -> Exposure {
    if to.0 > from.0 {
        Exposure((from.0 + units).min(to.0))
    } else {
        Exposure((from.0 - units).max(to.0))
    }
}

fn crosses_zero(allowed_target: f64, curve_target: f64) -> bool {
    (allowed_target > f64::EPSILON && curve_target < -f64::EPSILON)
        || (allowed_target < -f64::EPSILON && curve_target > f64::EPSILON)
}

fn inside_or_equal(curve_target: f64, allowed_target: f64) -> bool {
    if allowed_target > f64::EPSILON {
        curve_target <= allowed_target
    } else if allowed_target < -f64::EPSILON {
        curve_target >= allowed_target
    } else {
        curve_target.abs() <= f64::EPSILON
    }
}

fn same_direction_backlog(
    direction: RiskIncreaseDirection,
    allowed_target: f64,
    curve_target: f64,
) -> bool {
    match direction {
        RiskIncreaseDirection::Long => curve_target > allowed_target,
        RiskIncreaseDirection::Short => curve_target < allowed_target,
    }
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::RiskIncreaseDelayConfig;
    use poise_core::types::Exposure;

    use super::*;

    fn config() -> RiskIncreaseDelayConfig {
        RiskIncreaseDelayConfig {
            startup_initial_ratio: 0.3,
            advantage_min_rebalance_multiples: 2.0,
            base_step_min_rebalance_multiples: 1.0,
            max_step_min_rebalance_multiples: 4.0,
            catchup_ratio: 0.25,
        }
    }

    fn input(
        state: Option<RiskExposureGateState>,
        current_exposure: f64,
        curve_target: f64,
        price: f64,
    ) -> RiskExposureGateInput {
        RiskExposureGateInput {
            config: Some(config()),
            min_rebalance_units: 0.5,
            state,
            current_exposure: Exposure(current_exposure),
            curve_target: Exposure(curve_target),
            strategy_price: price,
        }
    }

    #[test]
    fn startup_allows_initial_ratio_and_keeps_backlog() {
        let decision = apply(input(None, 0.0, 5.0, 100.0));

        assert_eq!(decision.allowed_target, Exposure(1.5));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                allowed_target: Exposure(1.5),
                anchor_price: 100.0,
                anchor_curve_target: Exposure(5.0),
            })
        );
        assert!(decision.next_release.is_some());
    }

    #[test]
    fn does_not_release_before_advantage_target() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state.clone()), 1.5, 5.9, 99.6));

        assert_eq!(decision.allowed_target, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
    }

    #[test]
    fn releases_dynamic_step_after_advantage() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state), 1.5, 6.0, 99.5));

        assert_eq!(decision.allowed_target, Exposure(2.625));
        assert_eq!(
            decision.state,
            Some(RiskExposureGateState {
                allowed_target: Exposure(2.625),
                anchor_price: 99.5,
                anchor_curve_target: Exposure(6.0),
            })
        );
    }

    #[test]
    fn smaller_backlog_does_not_reduce_allowed_target() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state.clone()), 1.5, 4.0, 101.0));

        assert_eq!(decision.allowed_target, Exposure(1.5));
        assert_eq!(decision.state, Some(state));
    }

    #[test]
    fn curve_target_inside_allowed_target_reduces_immediately() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state), 1.5, 1.0, 102.0));

        assert_eq!(decision.allowed_target, Exposure(1.0));
        assert_eq!(decision.state, None);
        assert_eq!(decision.next_release, None);
    }

    #[test]
    fn cross_zero_reduces_to_flat_before_new_direction() {
        let state = RiskExposureGateState {
            allowed_target: Exposure(1.5),
            anchor_price: 100.0,
            anchor_curve_target: Exposure(5.0),
        };

        let decision = apply(input(Some(state), 1.5, -1.0, 105.0));

        assert_eq!(decision.allowed_target, Exposure(0.0));
        assert_eq!(decision.state, None);
        assert_eq!(decision.next_release, None);
    }

    #[test]
    fn disabled_config_follows_curve_target() {
        let decision = apply(RiskExposureGateInput {
            config: None,
            min_rebalance_units: 0.5,
            state: None,
            current_exposure: Exposure(0.0),
            curve_target: Exposure(5.0),
            strategy_price: 100.0,
        });

        assert_eq!(decision.allowed_target, Exposure(5.0));
        assert_eq!(decision.state, None);
        assert_eq!(decision.next_release, None);
    }
}
