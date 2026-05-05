use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exposure(pub f64);

impl Exposure {
    pub fn delta(&self, target: &Exposure) -> Exposure {
        Exposure(target.0 - self.0)
    }

    pub fn is_zero(&self) -> bool {
        self.0.abs() < f64::EPSILON
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn from_exposure(exposure: &Exposure) -> Option<Side> {
        if exposure.0 > f64::EPSILON {
            Some(Side::Buy)
        } else if exposure.0 < -f64::EPSILON {
            Some(Side::Sell)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeRules {
    pub price_tick: f64,
    #[serde(default)]
    pub price_precision: PricePrecision,
    pub quantity_step: f64,
    pub min_qty: f64,
    pub min_notional: f64,
    pub maker_fee_rate: f64,
    pub taker_fee_rate: f64,
}

impl ExchangeRules {
    pub fn round_price(&self, price: f64, rounding: PriceRounding) -> f64 {
        self.price_precision.round(price, rounding, self.price_tick)
    }

    pub fn prices_match(&self, left: f64, right: f64) -> bool {
        match self.price_precision {
            PricePrecision::FixedTick => values_match(left, right, self.price_tick),
            _ => {
                let left = self.round_price(left, PriceRounding::Nearest);
                let right = self.round_price(right, PriceRounding::Nearest);
                values_match(
                    left,
                    right,
                    self.price_precision.match_tolerance(left, right),
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceRounding {
    Down,
    Up,
    Nearest,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PricePrecision {
    FixedTick,
    SignificantFigures {
        max_decimals: u32,
        significant_figures: u32,
    },
}

impl Default for PricePrecision {
    fn default() -> Self {
        Self::FixedTick
    }
}

impl PricePrecision {
    pub fn significant_figures(max_decimals: u32, significant_figures: u32) -> Self {
        Self::SignificantFigures {
            max_decimals,
            significant_figures: significant_figures.max(1),
        }
    }

    pub fn round(self, price: f64, rounding: PriceRounding, fixed_tick: f64) -> f64 {
        if !price.is_finite() {
            return price;
        }

        let step = match self {
            Self::FixedTick => fixed_tick,
            Self::SignificantFigures {
                max_decimals,
                significant_figures,
            } => significant_figure_step(price, max_decimals, significant_figures),
        };
        round_to_price_step(price, step, rounding)
    }

    fn match_tolerance(self, left: f64, right: f64) -> f64 {
        match self {
            Self::FixedTick => f64::EPSILON,
            Self::SignificantFigures {
                max_decimals,
                significant_figures,
            } => {
                significant_figure_step(
                    left.abs().max(right.abs()),
                    max_decimals,
                    significant_figures,
                ) * 1e-9
            }
        }
    }
}

fn significant_figure_step(price: f64, max_decimals: u32, significant_figures: u32) -> f64 {
    if price.abs() <= f64::EPSILON {
        return 10_f64.powi(-(max_decimals as i32));
    }

    let magnitude = price.abs().log10().floor() as i32;
    let significant_scale = significant_figures as i32 - 1 - magnitude;
    let scale = (max_decimals as i32).min(significant_scale);
    10_f64.powi(-scale)
}

fn round_to_price_step(price: f64, step: f64, rounding: PriceRounding) -> f64 {
    if step <= f64::EPSILON {
        return price;
    }

    let scaled = price / step;
    let tolerance = scaled.abs().max(1.0) * f64::EPSILON * 16.0;
    let units = match rounding {
        PriceRounding::Down => (scaled + tolerance).floor(),
        PriceRounding::Up => (scaled - tolerance).ceil(),
        PriceRounding::Nearest => scaled.round(),
    };
    let rounded = units * step;
    if rounded == -0.0 { 0.0 } else { rounded }
}

fn values_match(left: f64, right: f64, tolerance: f64) -> bool {
    let tolerance = tolerance.max(f64::EPSILON);
    (left - right).abs() <= tolerance + f64::EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_arithmetic() {
        let a = Exposure(3.0);
        let b = Exposure(5.0);
        assert!((a.delta(&b).0 - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn side_from_exposure() {
        assert_eq!(Side::from_exposure(&Exposure(1.0)), Some(Side::Buy));
        assert_eq!(Side::from_exposure(&Exposure(-1.0)), Some(Side::Sell));
        assert_eq!(Side::from_exposure(&Exposure(0.0)), None);
    }

    #[test]
    fn side_serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&Side::Buy).unwrap(), "\"buy\"");
        assert_eq!(
            serde_json::from_str::<Side>("\"sell\"").unwrap(),
            Side::Sell
        );
    }

    #[test]
    fn exposure_is_zero() {
        assert!(Exposure(0.0).is_zero());
        assert!(!Exposure(1.0).is_zero());
        assert!(!Exposure(-0.001).is_zero());
    }

    #[test]
    fn significant_figure_precision_rounds_by_price_magnitude() {
        let precision = PricePrecision::significant_figures(2, 5);

        assert!((precision.round(1234.56, PriceRounding::Down, 0.0) - 1234.5).abs() < 1e-9);
        assert!((precision.round(1234.56, PriceRounding::Up, 0.0) - 1234.6).abs() < 1e-9);
        assert!((precision.round(123456.0, PriceRounding::Nearest, 0.0) - 123460.0).abs() < 1e-9);
    }
}
