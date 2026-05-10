use poise_core::strategy::TrackConfig;
use poise_core::types::Exposure;
use serde::{Deserialize, Serialize};

const EXPOSURE_ID_SCALE: f64 = 10_000.0;
const SOLVER_EPSILON: f64 = 1e-9;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProfileRevision(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BoundaryId {
    pub profile_revision: ProfileRevision,
    pub lower_exposure_bp: i64,
    pub upper_exposure_bp: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryBlueprint {
    pub id: BoundaryId,
    pub lower_exposure: Exposure,
    pub upper_exposure: Exposure,
    pub trigger_price: f64,
    pub step_size: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BoundaryOperation {
    pub boundary_id: BoundaryId,
    pub direction: BoundaryDirection,
}

pub fn discretize_boundaries(
    config: &TrackConfig,
    profile_revision: ProfileRevision,
) -> Vec<BoundaryBlueprint> {
    let lower_exposure = -config.short_exposure_units;
    let upper_exposure = config.long_exposure_units;
    let step_size = config.min_rebalance_units;
    if step_size <= f64::EPSILON || lower_exposure >= upper_exposure {
        return Vec::new();
    }

    let mut boundaries = Vec::new();
    let mut lower = lower_exposure;
    while lower < upper_exposure - SOLVER_EPSILON {
        let upper = (lower + step_size).min(upper_exposure);
        let id = BoundaryId {
            profile_revision: profile_revision.clone(),
            lower_exposure_bp: exposure_bp(lower),
            upper_exposure_bp: exposure_bp(upper),
        };
        boundaries.push(BoundaryBlueprint {
            id,
            lower_exposure: Exposure(lower),
            upper_exposure: Exposure(upper),
            trigger_price: trigger_price_for_boundary(upper, config),
            step_size: upper - lower,
        });
        lower = upper;
    }
    boundaries
}

pub fn profile_revision_for_config(config: &TrackConfig) -> ProfileRevision {
    ProfileRevision(
        serde_json::to_string(config)
            .expect("TrackConfig serialization should be deterministic and infallible"),
    )
}

pub fn trigger_price_for_boundary(boundary_upper_exposure: f64, config: &TrackConfig) -> f64 {
    let span = (config.long_exposure_units + config.short_exposure_units) / 2.0;
    if span <= f64::EPSILON {
        return config.band_center();
    }

    let bias = (config.long_exposure_units - config.short_exposure_units) / 2.0;
    let target =
        boundary_upper_exposure.clamp(-config.short_exposure_units, config.long_exposure_units);
    let mut low = config.lower_price;
    let mut high = config.upper_price;

    for _ in 0..80 {
        let mid = (low + high) / 2.0;
        let exposure = poise_core::strategy::desired_exposure(mid, config).0;
        if exposure > target {
            low = mid;
        } else {
            high = mid;
        }
    }

    let solved = (low + high) / 2.0;
    if !solved.is_finite() {
        config.band_center()
    } else if (target - (bias + span)).abs() < SOLVER_EPSILON {
        config.lower_price
    } else if (target - (bias - span)).abs() < SOLVER_EPSILON {
        config.upper_price
    } else {
        solved
    }
}

fn exposure_bp(exposure: f64) -> i64 {
    (exposure * EXPOSURE_ID_SCALE).round() as i64
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};

    use super::*;

    fn linear_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        }
    }

    #[test]
    fn discretize_boundaries_builds_adjacent_levels_across_full_curve_range() {
        let config = linear_config();
        let revision = ProfileRevision("rev-1".to_string());

        let boundaries = discretize_boundaries(&config, revision.clone());

        assert_eq!(boundaries.len(), 16);
        assert_eq!(boundaries.first().unwrap().lower_exposure.0, -8.0);
        assert_eq!(boundaries.first().unwrap().upper_exposure.0, -7.0);
        assert_eq!(boundaries.last().unwrap().lower_exposure.0, 7.0);
        assert_eq!(boundaries.last().unwrap().upper_exposure.0, 8.0);
        assert!(boundaries.iter().all(|boundary| {
            boundary.id.profile_revision == revision
                && (boundary.step_size - 1.0).abs() < f64::EPSILON
        }));
    }

    #[test]
    fn trigger_price_for_boundary_matches_linear_shape_boundary() {
        let config = linear_config();

        let price = trigger_price_for_boundary(2.0, &config);

        assert!((price - 97.5).abs() < 1e-9, "got {price}");
    }

    #[test]
    fn boundary_id_uses_profile_revision_and_adjacent_exposures_only() {
        let config = linear_config();
        let revision = ProfileRevision("rev-1".to_string());

        let boundaries = discretize_boundaries(&config, revision.clone());
        let boundary = boundaries
            .iter()
            .find(|boundary| {
                (boundary.lower_exposure.0 - 1.0).abs() < f64::EPSILON
                    && (boundary.upper_exposure.0 - 2.0).abs() < f64::EPSILON
            })
            .expect("boundary should exist");

        assert_eq!(boundary.id.profile_revision, revision);
        assert_eq!(boundary.id.lower_exposure_bp, 10_000);
        assert_eq!(boundary.id.upper_exposure_bp, 20_000);

        let operation = BoundaryOperation {
            boundary_id: boundary.id.clone(),
            direction: BoundaryDirection::Up,
        };
        assert_eq!(operation.boundary_id, boundary.id);
        assert_eq!(operation.direction, BoundaryDirection::Up);
    }

    #[test]
    fn profile_revision_for_config_is_deterministic_for_identical_configs() {
        let config = linear_config();

        let first = profile_revision_for_config(&config);
        let second = profile_revision_for_config(&config);

        assert_eq!(first, second);
        assert!(!first.0.is_empty());
    }
}
