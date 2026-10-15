//! 测试用的公共样例：设计第 3.2 节的做多网格。

use crate::Strategy;
use crate::config::{SideConfig, StrategyConfig};
use crate::instrument::{Fees, Instrument, InstrumentSpec, PriceRule};
use crate::units::{Notional, Price, Quantity, Rate, SCALE};

/// BTC：报价精度 1 美元，数量精度 0.001，最低订单金额 10 美元。
pub(crate) fn btc_spec() -> InstrumentSpec {
    InstrumentSpec::new(
        Instrument::try_new("BTC").unwrap(),
        Price::parse("1").unwrap(),
        Quantity::parse("0.001").unwrap(),
        Notional::parse("10").unwrap(),
    )
    .unwrap()
}

/// Hyperliquid 的 BTC 永续：价格最多 5 位有效数字（步长随价格变粗，数量精度给出下限
/// 0.1），数量精度 0.00001，最低订单金额 10 美元。
pub(crate) fn hl_btc_spec() -> InstrumentSpec {
    InstrumentSpec::with_price_rule(
        Instrument::try_new("BTC").unwrap(),
        PriceRule::SignificantFigures {
            max_figures: 5,
            step_floor: Price::parse("0.1").unwrap(),
        },
        Quantity::parse("0.00001").unwrap(),
        Notional::parse("10").unwrap(),
    )
    .unwrap()
}

/// 价格的有效数字位数，与 [`PriceRule`] 无关，用来在测试里独立核对交易所的
/// 「最多 5 位有效数字」。写成十进制后去掉前导零，剩下的位数就是它
/// （`95000` 是 5 位，`100005` 是 6 位，`0.00012` 是 2 位）。
pub(crate) fn significant_figures(price: Price) -> usize {
    let digits: String = price
        .to_string()
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    digits.trim_start_matches('0').len()
}

/// 交易所肯收的价格：整数价一律允许，非整数价要落在该价位的最小变动上，
/// 并且不超过 5 位有效数字。
pub(crate) fn is_quotable(spec: &InstrumentSpec, price: Price) -> bool {
    spec.price_rule().is_legal(price)
        && (price.units() % SCALE == 0 || significant_figures(price) <= 5)
}

pub(crate) fn btc_fees() -> Fees {
    Fees::new(
        Rate::parse("0.00045").unwrap(),
        Rate::parse("0.00015").unwrap(),
    )
}

/// 设计第 3.2 节的例子：`[75000, 95000]`，`M = 1`，`d = g = 1000`，没有主仓。
pub(crate) fn long_grid_config() -> StrategyConfig {
    StrategyConfig {
        instrument: Instrument::try_new("BTC").unwrap(),
        long: SideConfig {
            price_low: Price::parse("75000").unwrap(),
            price_high: Price::parse("95000").unwrap(),
            max_position: Quantity::parse("1").unwrap(),
            grid_spacing: Price::parse("1000").unwrap(),
            grid_spread: Price::parse("1000").unwrap(),
        },
    }
}

pub(crate) fn long_grid_strategy() -> Strategy {
    Strategy::new(long_grid_config(), btc_spec(), btc_fees()).unwrap()
}
