//! 测试用的公共样例：设计第 3.2 节的做多网格。

use crate::Strategy;
use crate::config::{SideConfig, StrategyConfig};
use crate::instrument::{Fees, Instrument, InstrumentSpec};
use crate::units::{Notional, Price, Quantity, Rate};

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
