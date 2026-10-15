//! 测试用的公共样例：设计第 3.2 节的做多网格，以及第 5 节的做空与中性网格。

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::Strategy;
use crate::config::{SideConfig, StrategyConfig};
use crate::instrument::{Fees, Instrument, InstrumentSpec, PriceRule};
use crate::units::{Notional, Price, Quantity, Rate, SCALE};

/// 测试用的固定时刻：2026-10-15 00:00:00 UTC 之后的第 `seconds` 秒。
pub(crate) fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap() + Duration::seconds(seconds)
}

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

/// 数量精度 0.00001 的同一标的：主仓部分成交的尾量也能用得上（第 4.2.3 节）。
pub(crate) fn fine_spec() -> InstrumentSpec {
    InstrumentSpec::new(
        Instrument::try_new("BTC").unwrap(),
        Price::parse("1").unwrap(),
        Quantity::parse("0.00001").unwrap(),
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

/// 报价精度 0.01 的同一标的：一个报价单位很小，用来构造第 6 节的下调。
pub(crate) fn cent_spec() -> InstrumentSpec {
    InstrumentSpec::new(
        Instrument::try_new("BTC").unwrap(),
        Price::parse("0.01").unwrap(),
        Quantity::parse("0.001").unwrap(),
        Notional::parse("10").unwrap(),
    )
    .unwrap()
}

/// 最低订单金额 1 美元的同一标的。
///
/// 设计第 13 节 B11 的中性网格价格在 79–96 之间、每格数量在 .05–.1 之间，每格金额
/// 不到 10 美元，在 HL 的最低金额下过不了每格金额校验。这份合约规则只把最低金额调小，
/// 好让验收用的价格与数量与设计完全一致。
pub(crate) fn cheap_spec() -> InstrumentSpec {
    InstrumentSpec::new(
        Instrument::try_new("BTC").unwrap(),
        Price::parse("1").unwrap(),
        Quantity::parse("0.001").unwrap(),
        Notional::parse("1").unwrap(),
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
        long: Some(SideConfig {
            price_low: Price::parse("75000").unwrap(),
            price_high: Price::parse("95000").unwrap(),
            max_position: Quantity::parse("1").unwrap(),
            core_ratio: Rate::ZERO,
            batch_count: None,
            grid_spacing: Price::parse("1000").unwrap(),
            grid_spread: Price::parse("1000").unwrap(),
            throttle_interval: None,
        }),
        short: None,
    }
}

/// 设计第 3.2 节的例子加上主仓：`r = .7`，`n = 4`。
///
/// 每批 `q = .25`，主仓 `C = .175`，网格 `G = .075`；每批 5 格，每格 `.015`。
pub(crate) fn core_long_config() -> StrategyConfig {
    StrategyConfig {
        long: Some(SideConfig {
            core_ratio: Rate::parse("0.7").unwrap(),
            batch_count: Some(4),
            ..long_grid_config().long.unwrap()
        }),
        ..long_grid_config()
    }
}

/// `r = 1`：只积累主仓，没有网格（第 2.3 节）。
///
/// `[75000, 95000]`，`M = 1`，`n = 4`：四批主仓价 95000、90000、85000、80000，
/// 每批 `q = C = .25`，没有格子，也没有网格循环。
pub(crate) fn core_only_config() -> StrategyConfig {
    StrategyConfig {
        long: Some(SideConfig {
            core_ratio: Rate::parse("1").unwrap(),
            batch_count: Some(4),
            ..long_grid_config().long.unwrap()
        }),
        ..long_grid_config()
    }
}

/// 做空网格：多头样例的镜像（第 5.1 节）。
///
/// 价格范围、数量、间距、价差都一样，只是方向相反：批次价从 `L` 往上，
/// 格子从 `L` 往上，平仓价在建仓价下面。
pub(crate) fn short_grid_config() -> StrategyConfig {
    StrategyConfig {
        long: None,
        short: long_grid_config().long,
        ..long_grid_config()
    }
}

/// 做空网格加上主仓：`r = .7`，`n = 4`。
pub(crate) fn core_short_config() -> StrategyConfig {
    StrategyConfig {
        short: Some(SideConfig {
            core_ratio: Rate::parse("0.7").unwrap(),
            batch_count: Some(4),
            ..short_grid_config().short.unwrap()
        }),
        ..short_grid_config()
    }
}

/// 中性网格（设计第 13 节 B11）：多头侧 79–89、`M = 1`，空头侧 91–96、`M = .25`，
/// `d = g = 1`，两侧都没有主仓。
///
/// 多头侧十格各 `.1`，空头侧五格各 `.05`；两侧之间空出 90 这一段。
pub(crate) fn neutral_grid_config() -> StrategyConfig {
    StrategyConfig {
        instrument: Instrument::try_new("BTC").unwrap(),
        long: Some(SideConfig {
            price_low: Price::parse("79").unwrap(),
            price_high: Price::parse("89").unwrap(),
            max_position: Quantity::parse("1").unwrap(),
            core_ratio: Rate::ZERO,
            batch_count: None,
            grid_spacing: Price::parse("1").unwrap(),
            grid_spread: Price::parse("1").unwrap(),
            throttle_interval: None,
        }),
        short: Some(SideConfig {
            price_low: Price::parse("91").unwrap(),
            price_high: Price::parse("96").unwrap(),
            max_position: Quantity::parse("0.25").unwrap(),
            core_ratio: Rate::ZERO,
            batch_count: None,
            grid_spacing: Price::parse("1").unwrap(),
            grid_spread: Price::parse("1").unwrap(),
            throttle_interval: None,
        }),
    }
}

pub(crate) fn long_grid_strategy() -> Strategy {
    Strategy::new(long_grid_config(), btc_spec(), btc_fees()).unwrap()
}

pub(crate) fn core_long_strategy() -> Strategy {
    Strategy::new(core_long_config(), btc_spec(), btc_fees()).unwrap()
}

/// 同样的批次划分，但数量精度是 0.00001，用来构造主仓尾量。
pub(crate) fn fine_core_long_strategy() -> Strategy {
    Strategy::new(core_long_config(), fine_spec(), btc_fees()).unwrap()
}

/// 做空网格策略：与 [`long_grid_strategy`] 同一份划分，方向相反。
pub(crate) fn short_grid_strategy() -> Strategy {
    Strategy::new(short_grid_config(), btc_spec(), btc_fees()).unwrap()
}

/// 做空网格加上主仓的策略：与 [`core_long_strategy`] 同一份划分，方向相反。
pub(crate) fn core_short_strategy() -> Strategy {
    Strategy::new(core_short_config(), btc_spec(), btc_fees()).unwrap()
}

/// 中性网格策略：两侧都没有主仓，格子自成单位。
pub(crate) fn neutral_grid_strategy() -> Strategy {
    Strategy::new(neutral_grid_config(), cheap_spec(), btc_fees()).unwrap()
}

/// 单批单格：`[99, 100]`，`M = .5`，`r = .22`，`n = 1`，`d = g = 1`。
///
/// `C = .11`，`G = .39`，只有一格。
pub(crate) fn single_small_batch_strategy() -> Strategy {
    let config = StrategyConfig {
        instrument: Instrument::try_new("BTC").unwrap(),
        long: Some(SideConfig {
            price_low: Price::parse("99").unwrap(),
            price_high: Price::parse("100").unwrap(),
            max_position: Quantity::parse("0.5").unwrap(),
            core_ratio: Rate::parse("0.22").unwrap(),
            batch_count: Some(1),
            grid_spacing: Price::parse("1").unwrap(),
            grid_spread: Price::parse("1").unwrap(),
            throttle_interval: None,
        }),
        short: None,
    };
    Strategy::new(config, btc_spec(), btc_fees()).unwrap()
}

/// 单批：`[97, 100]`，`M = 10`，`r = .7`，`n = 1`，`d = g = 1`。
///
/// `C = 7`，三格各 1，平仓价分别是 101、100、99。
pub(crate) fn single_batch_core_strategy() -> Strategy {
    let config = StrategyConfig {
        instrument: Instrument::try_new("BTC").unwrap(),
        long: Some(SideConfig {
            price_low: Price::parse("97").unwrap(),
            price_high: Price::parse("100").unwrap(),
            max_position: Quantity::parse("10").unwrap(),
            core_ratio: Rate::parse("0.7").unwrap(),
            batch_count: Some(1),
            grid_spacing: Price::parse("1").unwrap(),
            grid_spread: Price::parse("1").unwrap(),
            throttle_interval: None,
        }),
        short: None,
    };
    Strategy::new(config, cent_spec(), btc_fees()).unwrap()
}

/// 限速一小时、其余同 [`core_long_config`]：四批，每批五格。
pub(crate) fn throttled_core_long_strategy() -> Strategy {
    throttled_core_long_with_interval(Duration::hours(1))
}

/// 限速一小时、没有主仓的小网格：`r = 0`，二十格各自是限速单位。
pub(crate) fn throttled_long_grid_strategy() -> Strategy {
    let config = StrategyConfig {
        long: Some(SideConfig {
            throttle_interval: Some(Duration::hours(1)),
            ..long_grid_config().long.unwrap()
        }),
        ..long_grid_config()
    };
    Strategy::new(config, btc_spec(), btc_fees()).unwrap()
}

/// 与 [`throttled_core_long_strategy`] 同一划分，可以指定限速间隔。
pub(crate) fn throttled_core_long_with_interval(interval: Duration) -> Strategy {
    let config = StrategyConfig {
        long: Some(SideConfig {
            throttle_interval: Some(interval),
            ..core_long_config().long.unwrap()
        }),
        ..core_long_config()
    };
    Strategy::new(config, btc_spec(), btc_fees()).unwrap()
}

/// 没有主仓的小网格：`[90000, 95000]`，`M = .25`，`d = g = 1000`，五格各 .05。
pub(crate) fn five_cell_strategy() -> Strategy {
    let config = StrategyConfig {
        long: Some(SideConfig {
            price_low: Price::parse("90000").unwrap(),
            price_high: Price::parse("95000").unwrap(),
            max_position: Quantity::parse("0.25").unwrap(),
            ..long_grid_config().long.unwrap()
        }),
        ..long_grid_config()
    };
    Strategy::new(config, btc_spec(), btc_fees()).unwrap()
}
