//! 标的与合约规则：报价精度、数量精度、最低订单金额、费率。
//!
//! 这些规则来自交易所，不属于用户配置；配置校验和规划都用规整后的值判断。

use serde::{Deserialize, Serialize};

use crate::units::{Notional, Price, PriceRounding, Quantity, Rate, SCALE};

/// 标的，例如 Hyperliquid 主市场上的 `BTC`，或者第三方市场（HIP-3）上的 `xyz:GOLD`：
/// 冒号前是市场名，后面是这个市场里的标的。
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
        if let Some((dex, coin)) = symbol.split_once(':')
            && (dex.is_empty() || coin.is_empty() || coin.contains(':'))
        {
            return Err(SpecError::InvalidSymbol(symbol.to_string()));
        }
        Ok(Self(symbol.to_string()))
    }

    pub fn symbol(&self) -> &str {
        &self.0
    }

    /// 第三方市场（HIP-3）的市场名，例如 `xyz:GOLD` 的 `xyz`；主市场的标的是 `None`。
    pub fn dex(&self) -> Option<&str> {
        self.0.split_once(':').map(|(dex, _)| dex)
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

/// 报价规则：一个价位上允许的最小变动。
///
/// 多数交易所全价位共用一个步长；Hyperliquid 永续不是：价格最多 5 位有效数字，
/// 所以步长随价格变粗——8 万时是 1，十几万时是 10。规整和校验都要按「这个价格处的
/// 步长」算，按一个固定步长算出来的高价格子交易所不收。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PriceRule {
    /// 固定步长：任何价位都是同一个最小变动。
    Fixed(Price),
    /// 有效数字规则：最多 `max_figures` 位有效数字，并且步长不小于 `step_floor`
    /// （数量精度等其它约束带来的下限）。整数价格一律允许。
    SignificantFigures { max_figures: u32, step_floor: Price },
}

impl PriceRule {
    /// 这个价位上的最小变动。
    pub fn step_at(self, price: Price) -> Price {
        match self {
            Self::Fixed(step) => step,
            Self::SignificantFigures {
                max_figures,
                step_floor,
            } => {
                let figures = Price::power_of_ten(price.decade() - (max_figures.max(1) as i32 - 1));
                figures.max(step_floor)
            }
        }
    }

    /// 价格是不是落在交易所肯收的价位上。
    ///
    /// 有效数字规则下整数价格一律允许：`100005` 有 6 位数字，但交易所收。
    /// 我们自己产生的价格都是步长的整数倍，这一条只是不拒绝别人给的合法价。
    pub fn is_legal(self, price: Price) -> bool {
        if price.units() <= 0 {
            return false;
        }
        if matches!(self, Self::SignificantFigures { .. }) && price.units() % SCALE == 0 {
            return true;
        }
        price.units() % self.step_at(price).units() == 0
    }

    /// 按这个价格处的步长规整；规整结果超出可表示范围时返回 `None`。
    ///
    /// 向上规整可能越过可表示上限，所以不饱和：饱和出来的价格不是步长的整数倍，
    /// 会变成一张无法合法报出的订单。
    pub fn round(self, price: Price, rounding: PriceRounding) -> Option<Price> {
        price.checked_round_to_tick(self.step_at(price), rounding)
    }
}

/// 合约规则不合法。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("标的不能为空")]
    EmptySymbol,
    #[error("标的 `{0}` 的写法应为 `BTC` 或 `xyz:GOLD`")]
    InvalidSymbol(String),
    #[error("price tick must be positive, got {0}")]
    InvalidPriceTick(Price),
    #[error("a significant-figures price rule needs at least one figure and a positive step floor")]
    InvalidPriceRule,
    #[error("quantity step must be positive, got {0}")]
    InvalidQuantityStep(Quantity),
    #[error("minimum order notional must be positive, got {0}")]
    InvalidMinimumOrderNotional(Notional),
}

/// 一个标的的合约规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentSpec {
    instrument: Instrument,
    /// 报价规则。
    price_rule: PriceRule,
    quantity_step: Quantity,
    min_order_notional: Notional,
}

impl InstrumentSpec {
    /// 固定报价步长的合约规则。
    pub fn new(
        instrument: Instrument,
        price_tick: Price,
        quantity_step: Quantity,
        min_order_notional: Notional,
    ) -> Result<Self, SpecError> {
        Self::with_price_rule(
            instrument,
            PriceRule::Fixed(price_tick),
            quantity_step,
            min_order_notional,
        )
    }

    /// 按交易所给的报价规则构造。
    pub fn with_price_rule(
        instrument: Instrument,
        price_rule: PriceRule,
        quantity_step: Quantity,
        min_order_notional: Notional,
    ) -> Result<Self, SpecError> {
        match price_rule {
            PriceRule::Fixed(tick) if tick.units() <= 0 => {
                return Err(SpecError::InvalidPriceTick(tick));
            }
            PriceRule::SignificantFigures { max_figures: 0, .. } => {
                return Err(SpecError::InvalidPriceRule);
            }
            PriceRule::SignificantFigures { step_floor, .. } if step_floor.units() <= 0 => {
                return Err(SpecError::InvalidPriceRule);
            }
            _ => {}
        }
        if quantity_step.units() <= 0 {
            return Err(SpecError::InvalidQuantityStep(quantity_step));
        }
        if min_order_notional.raw() <= 0 {
            return Err(SpecError::InvalidMinimumOrderNotional(min_order_notional));
        }
        Ok(Self {
            instrument,
            price_rule,
            quantity_step,
            min_order_notional,
        })
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn price_rule(&self) -> PriceRule {
        self.price_rule
    }

    pub fn quantity_step(&self) -> Quantity {
        self.quantity_step
    }

    pub fn min_order_notional(&self) -> Notional {
        self.min_order_notional
    }

    /// 价格按这个价格处的报价精度规整；规整结果超出可表示范围时返回 `None`。
    pub fn normalize_price(&self, price: Price, rounding: PriceRounding) -> Option<Price> {
        self.price_rule.round(price, rounding)
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
    use super::{Instrument, InstrumentSpec, Notional, PriceRule, SpecError};
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

    /// 第三方市场的标的写成 `dex:COIN`；冒号两边都不能空，也只能有一个冒号。
    #[test]
    fn a_third_party_market_is_written_before_a_colon() {
        assert_eq!(Instrument::try_new("BTC").unwrap().dex(), None);
        let gold = Instrument::try_new("xyz:GOLD").unwrap();
        assert_eq!(gold.dex(), Some("xyz"));
        assert_eq!(gold.symbol(), "xyz:GOLD");
        for bad in [":GOLD", "xyz:", "xyz:GOLD:X"] {
            assert!(
                matches!(Instrument::try_new(bad), Err(SpecError::InvalidSymbol(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn instrument_and_spec_round_trip_through_json() {
        let spec = btc_spec();

        let json = serde_json::to_string(&spec).unwrap();

        assert!(json.contains("\"instrument\":\"BTC\""));
        assert!(json.contains("\"price_rule\":\"1\""));
        assert!(json.contains("\"quantity_step\":\"0.001\""));
        assert!(json.contains("\"min_order_notional\":\"10\""));
        assert_eq!(serde_json::from_str::<InstrumentSpec>(&json).unwrap(), spec);
        assert!(serde_json::from_str::<Instrument>("\"\"").is_err());
    }

    /// 有效数字规则也能存进数据库再读回来（枚举在 JSON 里是不带标签的）。
    #[test]
    fn a_significant_figures_spec_round_trips_through_json() {
        let spec = crate::fixtures::hl_btc_spec();

        let json = serde_json::to_string(&spec).unwrap();

        assert!(json.contains("\"price_rule\":{\"max_figures\":5,\"step_floor\":\"0.1\"}"));
        assert_eq!(serde_json::from_str::<InstrumentSpec>(&json).unwrap(), spec);
    }

    /// 有效数字规则的步长随价格变粗，整数价一律允许。
    #[test]
    fn a_significant_figures_rule_coarsens_the_step_as_the_price_grows() {
        let rule = PriceRule::SignificantFigures {
            max_figures: 5,
            step_floor: Price::parse("0.1").unwrap(),
        };

        // 8 万这一档是 1，十几万这一档是 10。
        assert_eq!(
            rule.step_at(Price::parse("95000").unwrap()),
            Price::parse("1").unwrap()
        );
        assert_eq!(
            rule.step_at(Price::parse("105000").unwrap()),
            Price::parse("10").unwrap()
        );
        // 有效数字允许 0.00001，但步长下限（数量精度）把它顶到 0.1。
        assert_eq!(
            rule.step_at(Price::parse("0.5").unwrap()),
            Price::parse("0.1").unwrap()
        );

        // 整数价一律允许：`100005` 有 6 位数字，交易所照样收。
        assert!(rule.is_legal(Price::parse("100005").unwrap()));
        assert!(rule.is_legal(Price::parse("95000").unwrap()));
        // 带小数就得落在步长上。
        assert!(!rule.is_legal(Price::parse("100005.5").unwrap()));
        assert!(!rule.is_legal(Price::parse("95001.5").unwrap()));
        assert!(!rule.is_legal(Price::ZERO));
    }

    /// 规整出来的价格必须自己能通过校验：否则策略算得出格子，却一张单也发不出去。
    #[test]
    fn rounding_always_lands_on_a_price_the_rule_accepts() {
        let rule = PriceRule::SignificantFigures {
            max_figures: 5,
            step_floor: Price::parse("0.1").unwrap(),
        };

        // 含跨十进位边界（99999.4、100004.9）和步长下限生效（0.34）的价格。
        for raw in ["99999.4", "100004", "100004.9", "0.34", "105000.7", "95000"] {
            let raw = Price::parse(raw).unwrap();
            for rounding in [
                PriceRounding::Down,
                PriceRounding::Up,
                PriceRounding::Nearest,
            ] {
                let rounded = rule.round(raw, rounding).unwrap();
                assert!(
                    rule.is_legal(rounded),
                    "{raw} 按 {rounding:?} 规整成 {rounded}，但它不是合法报价（步长 {}）",
                    rule.step_at(rounded)
                );
            }
        }
    }

    /// 固定步长的规则不随价格变，用来跑模拟交易所和其它交易所。
    #[test]
    fn a_fixed_rule_keeps_one_step_at_every_price() {
        let rule = PriceRule::Fixed(Price::parse("0.5").unwrap());

        assert_eq!(
            rule.step_at(Price::parse("95000").unwrap()),
            Price::parse("0.5").unwrap()
        );
        assert_eq!(
            rule.step_at(Price::parse("0.2").unwrap()),
            Price::parse("0.5").unwrap()
        );
        assert!(rule.is_legal(Price::parse("95000.5").unwrap()));
        assert!(!rule.is_legal(Price::parse("95000.4").unwrap()));
    }

    #[test]
    fn a_significant_figures_rule_rejects_impossible_settings() {
        let build = |rule| {
            InstrumentSpec::with_price_rule(
                Instrument::try_new("BTC").unwrap(),
                rule,
                Quantity::parse("0.001").unwrap(),
                Notional::parse("10").unwrap(),
            )
        };

        assert_eq!(
            build(PriceRule::SignificantFigures {
                max_figures: 0,
                step_floor: Price::parse("1").unwrap(),
            })
            .unwrap_err(),
            SpecError::InvalidPriceRule
        );
        assert_eq!(
            build(PriceRule::SignificantFigures {
                max_figures: 5,
                step_floor: Price::ZERO,
            })
            .unwrap_err(),
            SpecError::InvalidPriceRule
        );
    }
}
