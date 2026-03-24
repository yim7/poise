use serde::{Deserialize, Serialize};

use crate::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_capacity: f64,
    pub short_capacity: f64,
    pub capacity_notional: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShapeFamily {
    Linear,
    Convex,
    Concave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutOfBandPolicy {
    Freeze,
    ReduceOnly,
    Terminate,
    Hold,
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
pub enum BandBoundary {
    Below,
    Above,
}

pub fn validate_config(config: &GridConfig) -> Result<(), String> {
    if config.lower_price >= config.upper_price {
        return Err("lower_price must be less than upper_price".into());
    }
    if config.long_capacity < 0.0 || config.short_capacity < 0.0 {
        return Err("capacities must not be negative".into());
    }
    if config.long_capacity + config.short_capacity <= f64::EPSILON {
        return Err("at least one capacity must be positive".into());
    }
    if config.capacity_notional <= 0.0 {
        return Err("capacity_notional must be positive".into());
    }
    Ok(())
}

/// 纯函数：给定价格和配置，返回目标占用。
///
/// 使用策略族设计文档中定义的 g(x) = 1 - x^p 公式：
/// - Linear: p=1, g(x) = 1 - x
/// - Convex: p=2, g(x) = 1 - x²
/// - Concave: p=0.5, g(x) = 1 - √x
///
/// target = -short_capacity + (long_capacity + short_capacity) * g(x)
pub fn target_exposure(price: f64, config: &GridConfig) -> Exposure {
    let x =
        ((price - config.lower_price) / (config.upper_price - config.lower_price)).clamp(0.0, 1.0);
    let g = match config.shape_family {
        ShapeFamily::Linear => 1.0 - x,
        ShapeFamily::Convex => 1.0 - x.powi(2),
        ShapeFamily::Concave => 1.0 - x.sqrt(),
    };
    Exposure(-config.short_capacity + (config.long_capacity + config.short_capacity) * g)
}

pub fn band_status(price: f64, config: &GridConfig) -> BandStatus {
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
            target: target_exposure(price, config),
        }
    }
}

impl GridConfig {
    pub fn band_center(&self) -> f64 {
        (self.lower_price + self.upper_price) / 2.0
    }

    pub fn capacity_unit_qty(&self) -> f64 {
        let center = self.band_center();
        if center <= f64::EPSILON {
            0.0
        } else {
            self.capacity_notional / center
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neutral_config() -> GridConfig {
        GridConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_capacity: 8.0,
            short_capacity: 8.0,
            capacity_notional: 375.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn long_only_config() -> GridConfig {
        GridConfig {
            long_capacity: 8.0,
            short_capacity: 0.0,
            ..neutral_config()
        }
    }

    #[test]
    fn validate_rejects_inverted_prices() {
        let config = GridConfig {
            lower_price: 110.0,
            upper_price: 90.0,
            ..neutral_config()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_rejects_negative_capacity() {
        let config = GridConfig {
            long_capacity: -1.0,
            ..neutral_config()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_rejects_both_zero_capacity() {
        let config = GridConfig {
            long_capacity: 0.0,
            short_capacity: 0.0,
            ..neutral_config()
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn validate_accepts_valid_config() {
        assert!(validate_config(&neutral_config()).is_ok());
        assert!(validate_config(&long_only_config()).is_ok());
    }

    #[test]
    fn target_exposure_neutral_at_center() {
        let exposure = target_exposure(100.0, &neutral_config());
        assert!((exposure.0).abs() < 0.001);
    }

    #[test]
    fn target_exposure_full_long_at_lower() {
        let exposure = target_exposure(90.0, &neutral_config());
        assert!((exposure.0 - 8.0).abs() < 0.001);
    }

    #[test]
    fn target_exposure_full_short_at_upper() {
        let exposure = target_exposure(110.0, &neutral_config());
        assert!((exposure.0 + 8.0).abs() < 0.001);
    }

    #[test]
    fn target_exposure_long_only_zero_at_upper() {
        let exposure = target_exposure(110.0, &long_only_config());
        assert!((exposure.0).abs() < 0.001);
    }

    #[test]
    fn target_exposure_long_only_half_at_center() {
        let exposure = target_exposure(100.0, &long_only_config());
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
    fn convex_shape_slower_departure() {
        let config = GridConfig {
            shape_family: ShapeFamily::Convex,
            ..neutral_config()
        };
        let linear_mid = target_exposure(95.0, &neutral_config());
        let convex_mid = target_exposure(95.0, &config);
        assert!(convex_mid.0 > linear_mid.0);
    }

    #[test]
    fn capacity_unit_qty() {
        let config = neutral_config();
        let qty = config.capacity_unit_qty();
        assert!((qty - 3.75).abs() < 0.01); // 375 / 100
    }
}
