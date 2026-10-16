//! 合约规则：数量精度、报价精度、最低订单金额。
//!
//! Hyperliquid 永续的价格规则是「最多 5 位有效数字，且小数位不超过 `6 - szDecimals`，
//! 整数价格一律允许」。这不是一个固定的最小报价单位：同一张合约在不同价位上的最小变动
//! 不同（BTC 在 9 万时能报到 1，在十几万时只能报到 10）。
//!
//! 这里把它翻译成 [`PriceRule::SignificantFigures`] 交给策略：规整和校验用同一条规则，
//! 不必按某个标记价猜一个步长出来。下单前仍按这条规则再验一次，不合规就本地拒绝，
//! 不静默改成别的价格。

use poise_strategy::instrument::{Instrument, InstrumentSpec, PriceRule};
use poise_strategy::units::{Notional, Price, Quantity};

use crate::models::AssetMetaResponse;

/// 最低订单金额（美元）。交易所对每张单都查这个下限，减仓单也不例外。
pub(crate) const MIN_ORDER_NOTIONAL: &str = "10";

/// 价格最多 5 位有效数字。
const MAX_SIGNIFICANT_FIGURES: usize = 5;
/// 价格小数位不超过 `6 - szDecimals`。
const MAX_PRICE_DECIMALS_BASE: i32 = 6;

/// 一个标的的合约规则，来自 `/info` 的 `meta`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AssetMeta {
    /// 下单时用的资产序号（`meta.universe` 里的下标）。
    pub index: u32,
    pub sz_decimals: u32,
}

impl AssetMeta {
    pub(crate) fn from_response(index: u32, response: &AssetMetaResponse) -> Self {
        Self {
            index,
            sz_decimals: response.sz_decimals,
        }
    }

    /// 数量精度：`szDecimals` 位小数。
    pub(crate) fn quantity_step(&self) -> Quantity {
        Quantity::power_of_ten(-(self.sz_decimals as i32))
    }

    /// 交易所的报价规则：最多 5 位有效数字，小数位不超过 `6 - szDecimals`。
    ///
    /// 小数位的约束写成「步长下限」：`szDecimals` 是 5 时价格最小只能动 0.1，
    /// 是 8 时只能动 100。
    pub(crate) fn price_rule(&self) -> PriceRule {
        PriceRule::SignificantFigures {
            max_figures: MAX_SIGNIFICANT_FIGURES as u32,
            step_floor: Price::power_of_ten(self.sz_decimals as i32 - MAX_PRICE_DECIMALS_BASE),
        }
    }

    /// 合约规则。与标记价无关：报价规则本身就是随价格变化的。
    pub(crate) fn instrument_spec(&self, instrument: Instrument) -> Result<InstrumentSpec, String> {
        InstrumentSpec::with_price_rule(
            instrument,
            self.price_rule(),
            self.quantity_step(),
            Notional::parse(MIN_ORDER_NOTIONAL).expect("a literal amount parses"),
        )
        .map_err(|error| error.to_string())
    }

    /// 价格是否符合交易所的报价规则。
    pub(crate) fn check_price(&self, price: Price) -> Result<(), String> {
        if price.units() <= 0 {
            return Err(format!("price must be positive, got {price}"));
        }
        let rule = self.price_rule();
        if !rule.is_legal(price) {
            return Err(format!(
                "price {price} is not a multiple of {step}, the smallest step the exchange allows at this price (at most {MAX_SIGNIFICANT_FIGURES} significant figures, and no more decimals than the size precision allows)",
                step = rule.step_at(price)
            ));
        }
        Ok(())
    }

    /// 数量是否符合交易所的数量精度。
    pub(crate) fn check_quantity(&self, quantity: Quantity) -> Result<(), String> {
        if quantity.units() <= 0 {
            return Err(format!("quantity must be positive, got {quantity}"));
        }
        let decimals = decimals_of(quantity);
        if decimals > self.sz_decimals as usize {
            return Err(format!(
                "quantity {quantity} has {decimals} decimal places, the exchange allows at most {} for this instrument",
                self.sz_decimals
            ));
        }
        Ok(())
    }
}

fn decimals_of(value: impl std::fmt::Display) -> usize {
    let text = value.to_string();
    match text.split_once('.') {
        Some((_, fraction)) => fraction.len(),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{AssetMeta, MIN_ORDER_NOTIONAL};
    use poise_strategy::instrument::{Instrument, PriceRule};
    use poise_strategy::units::{Notional, Price, Quantity};

    fn btc() -> AssetMeta {
        AssetMeta {
            index: 3,
            sz_decimals: 5,
        }
    }

    fn eth() -> AssetMeta {
        AssetMeta {
            index: 1,
            sz_decimals: 4,
        }
    }

    /// 交易所的规则翻成报价规则：5 位有效数字，步长下限由 `szDecimals` 决定。
    #[test]
    fn the_price_rule_is_five_significant_figures_floored_by_the_size_precision() {
        assert_eq!(
            btc().price_rule(),
            PriceRule::SignificantFigures {
                max_figures: 5,
                step_floor: Price::parse("0.1").unwrap(),
            }
        );
        assert_eq!(
            eth().price_rule(),
            PriceRule::SignificantFigures {
                max_figures: 5,
                step_floor: Price::parse("0.01").unwrap(),
            }
        );
        // 数量精度到 8 位小数时，价格只能是 100 的整数倍。
        assert_eq!(
            AssetMeta {
                index: 0,
                sz_decimals: 8
            }
            .price_rule(),
            PriceRule::SignificantFigures {
                max_figures: 5,
                step_floor: Price::parse("100").unwrap(),
            }
        );
    }

    /// 本地校验不能拒绝步长自己推出来的价格：否则价格一过十万美元，
    /// 每一格都算得出、却一张单都发不出去。
    #[test]
    fn every_price_the_step_produces_passes_the_check() {
        let asset = btc();
        for mark in ["95000", "104790", "280000"] {
            let mark = Price::parse(mark).unwrap();
            for step in 0..40 {
                // 每一格都按它自己价位上的步长走，和策略里的走法一致。
                let price = Price::from_units(mark.units());
                let price = (0..step).fold(price, |price, _| {
                    Price::from_units(price.units() - asset.price_rule().step_at(price).units())
                });
                assert!(
                    asset.check_price(price).is_ok(),
                    "price {price} (step {} at mark {mark}) was refused: {}",
                    asset.price_rule().step_at(price),
                    asset.check_price(price).unwrap_err()
                );
            }
        }
    }

    /// 步长随价格变粗：8 万时是 1，十几万时是 10。
    #[test]
    fn the_price_step_follows_the_price_magnitude() {
        let rule = btc().price_rule();

        assert_eq!(
            rule.step_at(Price::parse("95000").unwrap()),
            Price::parse("1").unwrap()
        );
        // BTC 在 9 千附近：5 位有效数字落在 0.1。
        assert_eq!(
            rule.step_at(Price::parse("9500").unwrap()),
            Price::parse("0.1").unwrap()
        );
        // 十万美元以上：5 位有效数字落在十位。
        assert_eq!(
            rule.step_at(Price::parse("100190").unwrap()),
            Price::parse("10").unwrap()
        );
        assert_eq!(
            rule.step_at(Price::parse("280000").unwrap()),
            Price::parse("10").unwrap()
        );
        // 再往上是百位。
        assert_eq!(
            rule.step_at(Price::parse("1200000").unwrap()),
            Price::parse("100").unwrap()
        );

        // ETH 在 2500 附近：数量精度只允许 0.01，有效数字要求 0.1，取更大的那个。
        assert_eq!(
            eth().price_rule().step_at(Price::parse("2500").unwrap()),
            Price::parse("0.1").unwrap()
        );
    }

    #[test]
    fn quantity_step_is_the_size_precision() {
        assert_eq!(btc().quantity_step(), Quantity::parse("0.00001").unwrap());
        assert_eq!(eth().quantity_step(), Quantity::parse("0.0001").unwrap());
    }

    #[test]
    fn instrument_spec_carries_the_rule_step_and_minimum() {
        let spec = btc()
            .instrument_spec(Instrument::try_new("BTC").unwrap())
            .unwrap();

        assert_eq!(spec.price_rule(), btc().price_rule());
        assert_eq!(spec.quantity_step(), Quantity::parse("0.00001").unwrap());
        assert_eq!(
            spec.min_order_notional(),
            Notional::parse(MIN_ORDER_NOTIONAL).unwrap()
        );
        assert!(spec.meets_minimum_order(
            Quantity::parse("0.00011").unwrap(),
            Price::parse("95000").unwrap()
        ));
        assert!(!spec.meets_minimum_order(
            Quantity::parse("0.00001").unwrap(),
            Price::parse("95000").unwrap()
        ));
    }

    #[test]
    fn refuses_prices_the_exchange_would_refuse() {
        assert!(btc().check_price(Price::parse("95001").unwrap()).is_ok());
        // 5 位有效数字落到小数位上也合规。
        assert!(btc().check_price(Price::parse("9500.1").unwrap()).is_ok());
        // ETH 的数量精度只到 0.01，再细的价不收。
        assert!(
            eth()
                .check_price(Price::parse("95.555").unwrap())
                .unwrap_err()
                .contains("smallest step")
        );
        // BTC 在 9 万这一档，0.5 的价不收。
        assert!(
            btc()
                .check_price(Price::parse("95001.5").unwrap())
                .unwrap_err()
                .contains("smallest step")
        );
        assert!(
            btc()
                .check_price(Price::parse("0").unwrap())
                .unwrap_err()
                .contains("must be positive")
        );
    }

    /// 十万元以上的价只收 10 的整数倍，但整数价一律允许：
    /// `100005` 有 6 位数字，交易所照样收，本地不能把它挡下来。
    #[test]
    fn a_price_one_decade_above_the_mark_needs_a_coarser_step() {
        let asset = btc();

        assert!(asset.check_price(Price::parse("99999").unwrap()).is_ok());
        assert!(asset.check_price(Price::parse("100190").unwrap()).is_ok());
        assert!(asset.check_price(Price::parse("100005").unwrap()).is_ok());
        // 带小数就不是整数了，得落在步长上。
        assert!(
            asset
                .check_price(Price::parse("100005.5").unwrap())
                .unwrap_err()
                .contains("smallest step")
        );
    }

    #[test]
    fn refuses_sizes_the_exchange_would_refuse() {
        assert!(
            btc()
                .check_quantity(Quantity::parse("0.00011").unwrap())
                .is_ok()
        );
        assert!(
            btc()
                .check_quantity(Quantity::parse("0.000115").unwrap())
                .unwrap_err()
                .contains("decimal places")
        );
        assert!(
            btc()
                .check_quantity(Quantity::ZERO)
                .unwrap_err()
                .contains("must be positive")
        );
    }
}
