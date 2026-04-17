use serde::{Deserialize, Serialize};

use crate::types::Exposure;

pub const DEFAULT_MIN_REBALANCE_UNITS: f64 = 0.5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    #[serde(default = "default_min_rebalance_units")]
    pub min_rebalance_units: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeFamily {
    Linear,
    Inertial,
    Responsive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutOfBandPolicy {
    Freeze,
    Hold,
    Flatten,
    Terminate,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BandStatus {
    InBand {
        target: Exposure,
    },
    OutOfBand {
        policy: OutOfBandPolicy,
        boundary: BandBoundary,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandBoundary {
    Below,
    Above,
}

pub fn validate_config(config: &TrackConfig) -> Result<(), String> {
    if config.lower_price >= config.upper_price {
        return Err("lower_price must be less than upper_price".into());
    }
    if config.long_exposure_units < 0.0 || config.short_exposure_units < 0.0 {
        return Err("capacities must not be negative".into());
    }
    if config.long_exposure_units + config.short_exposure_units <= f64::EPSILON {
        return Err("at least one capacity must be positive".into());
    }
    if config.notional_per_unit <= 0.0 {
        return Err("notional_per_unit must be positive".into());
    }
    if !config.min_rebalance_units.is_finite() {
        return Err("min_rebalance_units must be finite".into());
    }
    if config.min_rebalance_units < 0.0 {
        return Err("min_rebalance_units must not be negative".into());
    }
    Ok(())
}

fn default_min_rebalance_units() -> f64 {
    DEFAULT_MIN_REBALANCE_UNITS
}

/// 纯函数：给定价格和配置，返回目标占用。
///
/// 使用围绕价格带中点对称的控仓曲线：
/// - Linear:      h(u) = -sign(u) * |u|
/// - Inertial:    h(u) = -sign(u) * |u|^0.65
/// - Responsive:  h(u) = -sign(u) * |u|^1.6
pub fn desired_exposure(price: f64, config: &TrackConfig) -> Exposure {
    let position = signed_band_position(price, config);
    let span = (config.long_exposure_units + config.short_exposure_units) / 2.0;
    let bias = (config.long_exposure_units - config.short_exposure_units) / 2.0;

    Exposure(bias + span * mirrored_shape_value(position, config.shape_family))
}

fn signed_band_position(price: f64, config: &TrackConfig) -> f64 {
    let half_band = (config.upper_price - config.lower_price) / 2.0;
    ((price - config.band_center()) / half_band).clamp(-1.0, 1.0)
}

fn mirrored_shape_value(position: f64, shape_family: ShapeFamily) -> f64 {
    let exponent = shape_family_exponent(shape_family);
    let magnitude = position.abs().powf(exponent);

    if position >= 0.0 {
        -magnitude
    } else {
        magnitude
    }
}

fn shape_family_exponent(shape_family: ShapeFamily) -> f64 {
    match shape_family {
        ShapeFamily::Linear => 1.0,
        ShapeFamily::Inertial => 0.65,
        ShapeFamily::Responsive => 1.6,
    }
}

pub fn band_status(price: f64, config: &TrackConfig) -> BandStatus {
    if price < config.lower_price - f64::EPSILON {
        BandStatus::OutOfBand {
            policy: config.out_of_band_policy,
            boundary: BandBoundary::Below,
        }
    } else if price > config.upper_price + f64::EPSILON {
        BandStatus::OutOfBand {
            policy: config.out_of_band_policy,
            boundary: BandBoundary::Above,
        }
    } else {
        BandStatus::InBand {
            target: desired_exposure(price, config),
        }
    }
}

impl TrackConfig {
    pub fn band_center(&self) -> f64 {
        (self.lower_price + self.upper_price) / 2.0
    }

    pub fn base_qty_per_unit(&self) -> f64 {
        let center = self.band_center();
        if center <= f64::EPSILON {
            0.0
        } else {
            self.notional_per_unit / center
        }
    }

    pub fn exposure_from_position_qty(&self, qty: f64) -> Exposure {
        let unit_qty = self.base_qty_per_unit();
        if !unit_qty.is_finite() || unit_qty <= f64::EPSILON {
            Exposure(0.0)
        } else {
            Exposure(qty / unit_qty)
        }
    }

    pub fn abs_notional_from_position_qty(&self, qty: f64) -> f64 {
        self.exposure_from_position_qty(qty).0.abs() * self.notional_per_unit
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct ShapeFamilyExponentFile {
        linear: f64,
        inertial: f64,
        responsive: f64,
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.02,
            "expected {expected}, got {actual}"
        );
    }

    fn neutral_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn long_only_config() -> TrackConfig {
        TrackConfig {
            long_exposure_units: 8.0,
            short_exposure_units: 0.0,
            ..neutral_config()
        }
    }

    #[test]
    fn validate_rejects_inverted_prices() {
        let config = TrackConfig {
            lower_price: 110.0,
            upper_price: 90.0,
            ..neutral_config()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_rejects_negative_capacity() {
        let config = TrackConfig {
            long_exposure_units: -1.0,
            ..neutral_config()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_rejects_both_zero_capacity() {
        let config = TrackConfig {
            long_exposure_units: 0.0,
            short_exposure_units: 0.0,
            ..neutral_config()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn out_of_band_policy_serializes_flatten() {
        let policy: OutOfBandPolicy = serde_json::from_str("\"flatten\"").unwrap();

        assert_eq!(serde_json::to_string(&policy).unwrap(), "\"flatten\"");
    }

    #[test]
    fn validate_accepts_valid_config() {
        assert!(validate_config(&neutral_config()).is_ok());
        assert!(validate_config(&long_only_config()).is_ok());
    }

    #[test]
    fn validate_rejects_negative_min_rebalance_units() {
        let config = TrackConfig {
            min_rebalance_units: -0.1,
            ..neutral_config()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_rejects_non_finite_min_rebalance_units() {
        let config = TrackConfig {
            min_rebalance_units: f64::NAN,
            ..neutral_config()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn desired_exposure_neutral_at_center() {
        let exposure = desired_exposure(100.0, &neutral_config());
        assert!((exposure.0).abs() < 0.001);
    }

    #[test]
    fn desired_exposure_full_long_at_lower() {
        let exposure = desired_exposure(90.0, &neutral_config());
        assert!((exposure.0 - 8.0).abs() < 0.001);
    }

    #[test]
    fn desired_exposure_full_short_at_upper() {
        let exposure = desired_exposure(110.0, &neutral_config());
        assert!((exposure.0 + 8.0).abs() < 0.001);
    }

    #[test]
    fn desired_exposure_long_only_zero_at_upper() {
        let exposure = desired_exposure(110.0, &long_only_config());
        assert!((exposure.0).abs() < 0.001);
    }

    #[test]
    fn desired_exposure_long_only_half_at_center() {
        let exposure = desired_exposure(100.0, &long_only_config());
        assert!((exposure.0 - 4.0).abs() < 0.001);
    }

    #[test]
    fn band_status_in_band() {
        let status = band_status(100.0, &neutral_config());
        assert!(matches!(status, BandStatus::InBand { .. }));
    }

    #[test]
    fn band_status_below() {
        let status = band_status(85.0, &neutral_config());
        assert!(matches!(
            status,
            BandStatus::OutOfBand {
                boundary: BandBoundary::Below,
                ..
            }
        ));
    }

    #[test]
    fn band_status_above() {
        let status = band_status(115.0, &neutral_config());
        assert!(matches!(
            status,
            BandStatus::OutOfBand {
                boundary: BandBoundary::Above,
                ..
            }
        ));
    }

    #[test]
    fn neutral_curve_is_symmetric_around_center_for_every_shape_family() {
        for shape_family in [
            ShapeFamily::Linear,
            ShapeFamily::Inertial,
            ShapeFamily::Responsive,
        ] {
            let config = TrackConfig {
                shape_family,
                ..neutral_config()
            };

            let lower_side = desired_exposure(95.0, &config).0;
            let upper_side = desired_exposure(105.0, &config).0;

            assert_close(lower_side, -upper_side);
        }
    }

    #[test]
    fn biased_curve_shifts_center_by_capacity_difference() {
        let config = TrackConfig {
            long_exposure_units: 10.0,
            short_exposure_units: 6.0,
            ..neutral_config()
        };

        assert_close(desired_exposure(100.0, &config).0, 2.0);
        assert_close(desired_exposure(90.0, &config).0, 10.0);
        assert_close(desired_exposure(110.0, &config).0, -6.0);
    }

    #[test]
    fn stronger_shape_family_curves_have_tuned_inventory_separation_halfway_to_center() {
        let inertial = desired_exposure(
            95.0,
            &TrackConfig {
                shape_family: ShapeFamily::Inertial,
                ..neutral_config()
            },
        );
        let linear = desired_exposure(95.0, &neutral_config());
        let responsive = desired_exposure(
            95.0,
            &TrackConfig {
                shape_family: ShapeFamily::Responsive,
                ..neutral_config()
            },
        );

        assert_close(inertial.0, 5.10);
        assert_close(linear.0, 4.0);
        assert_close(responsive.0, 2.64);
        assert!(inertial.0 > linear.0);
        assert!(linear.0 > responsive.0);
    }

    #[test]
    fn shape_family_exponent_file_matches_strategy() {
        let parameters: ShapeFamilyExponentFile =
            serde_json::from_str(include_str!("../shape_family_exponents.json")).unwrap();

        assert_close(
            parameters.linear,
            shape_family_exponent(ShapeFamily::Linear),
        );
        assert_close(
            parameters.inertial,
            shape_family_exponent(ShapeFamily::Inertial),
        );
        assert_close(
            parameters.responsive,
            shape_family_exponent(ShapeFamily::Responsive),
        );
    }

    #[test]
    fn base_qty_per_unit() {
        let config = neutral_config();
        let qty = config.base_qty_per_unit();
        assert!((qty - 3.75).abs() < 0.01); // 375 / 100
    }

    #[test]
    fn exposure_from_position_qty_uses_base_qty_per_unit() {
        let config = neutral_config();

        let exposure = config.exposure_from_position_qty(195.0);

        assert!((exposure.0 - 52.0).abs() < 0.01);
    }

    #[test]
    fn abs_notional_from_position_qty_reuses_exposure_conversion() {
        let config = neutral_config();

        let notional = config.abs_notional_from_position_qty(195.0);

        assert!((notional - 19_500.0).abs() < 0.01);
    }
}
