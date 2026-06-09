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
    #[serde(default)]
    pub quantity_kind: QuantityKind,
    #[serde(default)]
    pub contract_notional: Option<f64>,
    #[serde(default)]
    pub settlement_asset: String,
    pub quantity_step: f64,
    pub min_qty: f64,
    pub min_notional: f64,
    pub maker_fee_rate: f64,
    pub taker_fee_rate: f64,
}

impl ExchangeRules {
    pub fn native_qty_per_exposure_unit(&self, notional_per_unit: f64, band_center: f64) -> f64 {
        if !notional_per_unit.is_finite() || notional_per_unit <= f64::EPSILON {
            return 0.0;
        }

        match self.quantity_kind {
            QuantityKind::BaseAsset => {
                if !band_center.is_finite() || band_center <= f64::EPSILON {
                    0.0
                } else {
                    notional_per_unit / band_center
                }
            }
            QuantityKind::InverseContract => self
                .contract_notional
                .filter(|contract_notional| {
                    contract_notional.is_finite() && *contract_notional > f64::EPSILON
                })
                .map_or(0.0, |contract_notional| {
                    notional_per_unit / contract_notional
                }),
        }
    }

    pub fn notional_from_native_qty(&self, native_qty: f64, price: f64) -> f64 {
        match self.quantity_kind {
            QuantityKind::BaseAsset => {
                if !price.is_finite() || price <= f64::EPSILON {
                    0.0
                } else {
                    native_qty.abs() * price
                }
            }
            QuantityKind::InverseContract => self
                .contract_notional
                .filter(|contract_notional| {
                    contract_notional.is_finite() && *contract_notional > f64::EPSILON
                })
                .map_or(0.0, |contract_notional| {
                    native_qty.abs() * contract_notional
                }),
        }
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuantityKind {
    BaseAsset,
    InverseContract,
}

impl Default for QuantityKind {
    fn default() -> Self {
        Self::BaseAsset
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
    fn base_asset_quantity_uses_band_center_for_unit_size() {
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: QuantityKind::BaseAsset,
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
            quantity_step: 0.001,
            min_qty: 0.001,
            min_notional: 5.0,
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.0004,
        };

        assert!((rules.native_qty_per_exposure_unit(1000.0, 100000.0) - 0.01).abs() < 1e-12);
        assert!((rules.notional_from_native_qty(-0.03, 50000.0) - 1500.0).abs() < 1e-9);
    }

    #[test]
    fn inverse_contract_quantity_uses_contract_notional_for_unit_size() {
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: QuantityKind::InverseContract,
            contract_notional: Some(100.0),
            settlement_asset: "BTC".to_string(),
            quantity_step: 0.1,
            min_qty: 0.1,
            min_notional: 0.0,
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.0005,
        };

        assert!((rules.native_qty_per_exposure_unit(1000.0, 100000.0) - 10.0).abs() < 1e-12);
        assert!((rules.notional_from_native_qty(-30.0, 100000.0) - 3000.0).abs() < 1e-9);
        assert!((rules.notional_from_native_qty(-30.0, 50000.0) - 3000.0).abs() < 1e-9);
    }

    #[test]
    fn inverse_contract_quantity_without_contract_notional_is_safe() {
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: QuantityKind::InverseContract,
            contract_notional: None,
            settlement_asset: "BTC".to_string(),
            quantity_step: 0.1,
            min_qty: 0.1,
            min_notional: 0.0,
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.0005,
        };

        assert_eq!(rules.native_qty_per_exposure_unit(1000.0, 100000.0), 0.0);
        assert_eq!(rules.notional_from_native_qty(-30.0, 100000.0), 0.0);
    }

    #[test]
    fn significant_figure_precision_rounds_by_price_magnitude() {
        let precision = PricePrecision::significant_figures(2, 5);

        assert!((precision.round(1234.56, PriceRounding::Down, 0.0) - 1234.5).abs() < 1e-9);
        assert!((precision.round(1234.56, PriceRounding::Up, 0.0) - 1234.6).abs() < 1e-9);
        assert!((precision.round(123456.0, PriceRounding::Nearest, 0.0) - 123460.0).abs() < 1e-9);
    }
}
