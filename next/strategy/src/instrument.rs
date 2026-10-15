//! 标的与合约规则：报价精度、数量精度、最低订单金额、费率。
//!
//! 这些规则来自交易所，不属于用户配置；配置校验和规划都用规整后的值判断。

use serde::{Deserialize, Serialize};

use crate::units::{Notional, Price, PriceRounding, Quantity, Rate};

/// 标的，例如 Hyperliquid 上的 `BTC`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Instrument(String);

impl Instrument {
    pub fn try_new(symbol: impl Into<String>) -> Result<Self, SpecError> {
        let symbol = symbol.into();
        let symbol = symbol.trim();
        if symbol.is_empty() {
            return Err(SpecError::EmptySymbol);
        }
        Ok(Self(symbol.to_string()))
    }

    pub fn symbol(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Instrument {
    type Error = SpecError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl std::fmt::Display for Instrument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<Instrument> for String {
    fn from(instrument: Instrument) -> String {
        instrument.0
    }
}

/// 合约规则不合法。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("instrument symbol must not be empty")]
    EmptySymbol,
    #[error("price tick must be positive, got {0}")]
    InvalidPriceTick(Price),
    #[error("quantity step must be positive, got {0}")]
    InvalidQuantityStep(Quantity),
    #[error("minimum order notional must be positive, got {0}")]
    InvalidMinimumOrderNotional(Notional),
}

/// 一个标的的合约规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentSpec {
    instrument: Instrument,
    price_tick: Price,
    quantity_step: Quantity,
    min_order_notional: Notional,
}

impl InstrumentSpec {
    pub fn new(
        instrument: Instrument,
        price_tick: Price,
        quantity_step: Quantity,
        min_order_notional: Notional,
    ) -> Result<Self, SpecError> {
        if price_tick.units() <= 0 {
            return Err(SpecError::InvalidPriceTick(price_tick));
        }
        if quantity_step.units() <= 0 {
            return Err(SpecError::InvalidQuantityStep(quantity_step));
        }
        if min_order_notional.raw() <= 0 {
            return Err(SpecError::InvalidMinimumOrderNotional(min_order_notional));
        }
        Ok(Self {
            instrument,
            price_tick,
            quantity_step,
            min_order_notional,
        })
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn price_tick(&self) -> Price {
        self.price_tick
    }

    pub fn quantity_step(&self) -> Quantity {
        self.quantity_step
    }

    pub fn min_order_notional(&self) -> Notional {
        self.min_order_notional
    }

    /// 价格按报价精度规整；规整结果超出可表示范围时返回 `None`。
    pub fn normalize_price(&self, price: Price, rounding: PriceRounding) -> Option<Price> {
        price.checked_round_to_tick(self.price_tick, rounding)
    }

    /// 数量按数量精度向下规整。
    pub fn normalize_quantity(&self, quantity: Quantity) -> Quantity {
        quantity.floor_to_step(self.quantity_step)
    }

    /// 一张订单是否达到最低订单金额。
    pub fn meets_minimum_order(&self, quantity: Quantity, price: Price) -> bool {
        quantity.units() > 0 && Notional::of(quantity, price) >= self.min_order_notional
    }
}

/// 账户费率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fees {
    pub taker_rate: Rate,
    pub maker_rate: Rate,
}

impl Fees {
    pub fn new(taker_rate: Rate, maker_rate: Rate) -> Self {
        Self {
            taker_rate,
            maker_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Instrument, InstrumentSpec, Notional, SpecError};
    use crate::fixtures::btc_spec;
    use crate::units::{Price, PriceRounding, Quantity};

    #[test]
    fn spec_normalizes_price_to_tick_and_quantity_down_to_step() {
        let spec = btc_spec();

        assert_eq!(
            spec.normalize_price(Price::parse("94321.6").unwrap(), PriceRounding::Down),
            Some(Price::parse("94321").unwrap())
        );
        assert_eq!(
            spec.normalize_quantity(Quantity::parse("0.0159").unwrap()),
            Quantity::parse("0.015").unwrap()
        );
    }

    #[test]
    fn spec_compares_notional_against_minimum_order_amount() {
        let spec = btc_spec();

        assert!(!spec.meets_minimum_order(
            Quantity::parse("0.0001").unwrap(),
            Price::parse("76000").unwrap()
        ));
        assert!(spec.meets_minimum_order(
            Quantity::parse("0.001").unwrap(),
            Price::parse("76000").unwrap()
        ));
        assert!(!spec.meets_minimum_order(Quantity::ZERO, Price::parse("76000").unwrap()));
    }

    #[test]
    fn spec_rejects_invalid_rules() {
        assert_eq!(
            InstrumentSpec::new(
                Instrument::try_new("BTC").unwrap(),
                Price::ZERO,
                Quantity::parse("0.001").unwrap(),
                Notional::parse("10").unwrap(),
            )
            .unwrap_err(),
            SpecError::InvalidPriceTick(Price::ZERO)
        );
        assert_eq!(
            Instrument::try_new("  ").unwrap_err(),
            SpecError::EmptySymbol
        );
    }

    #[test]
    fn instrument_and_spec_round_trip_through_json() {
        let spec = btc_spec();

        let json = serde_json::to_string(&spec).unwrap();

        assert!(json.contains("\"instrument\":\"BTC\""));
        assert!(json.contains("\"price_tick\":\"1\""));
        assert!(json.contains("\"quantity_step\":\"0.001\""));
        assert!(json.contains("\"min_order_notional\":\"10\""));
        assert_eq!(serde_json::from_str::<InstrumentSpec>(&json).unwrap(), spec);
        assert!(serde_json::from_str::<Instrument>("\"\"").is_err());
    }
}
