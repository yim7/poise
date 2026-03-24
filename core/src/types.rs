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
    pub quantity_step: f64,
    pub min_qty: f64,
    pub min_notional: f64,
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
    fn exposure_is_zero() {
        assert!(Exposure(0.0).is_zero());
        assert!(!Exposure(1.0).is_zero());
        assert!(!Exposure(-0.001).is_zero());
    }
}
