//! 格子划分。
//!
//! 多头侧的格子买价从最高价 `H` 开始，每隔 `d` 一格，直到高于 `L` 的最后一格；
//! 每格卖价 = 买价 + `g`。格子按下标排列，0 是最靠近空隙（最高价）的一格，
//! 也是启动顺序里的第一个。
//!
//! 格子数量、最低订单金额和价差盈利性都算在生成这一侧：它们决定了哪些配置根本
//! 挂不出单，属于划分本身的性质，不是配置字段的格式问题。

use serde::{Deserialize, Serialize};

use crate::batch::BatchIndex;
use crate::error::ConfigError;
use crate::instrument::InstrumentSpec;
use crate::units::{Notional, Price, Quantity, Rate, SCALE, div_ceil, split_steps};

/// 格子在网格中的下标，0 是最靠近空隙的一格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CellIndex(pub usize);

impl std::fmt::Display for CellIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub index: CellIndex,
    pub buy_price: Price,
    pub sell_price: Price,
    /// 本格的网格数量：所在批次的网格部分平均分给跟随它的格子。
    pub quantity: Quantity,
    /// 跟随哪一批；主仓比例为 0 时没有批次，格子自成单位。
    pub batch: Option<BatchIndex>,
}

/// 一个策略的格子划分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridPlan {
    cells: Vec<Cell>,
}

impl GridPlan {
    pub(crate) fn from_cells(cells: Vec<Cell>) -> Self {
        Self { cells }
    }

    /// 按价格从高到低排列。
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn cell(&self, index: CellIndex) -> Option<&Cell> {
        self.cells.get(index.0)
    }
}

/// 价格范围与间距决定多少格。
///
/// 先算格数、再决定要不要构造价格，避免极端配置在分配内存时就把进程拖死。
pub(crate) fn cell_count(high: Price, low: Price, spacing: Price) -> i64 {
    debug_assert!(high.units() > low.units());
    debug_assert!(spacing.units() > 0);
    let span = (high.units() - low.units()) as i128;
    div_ceil(span, spacing.units() as i128) as i64
}

/// 从 `H` 开始每隔 `spacing` 一格，直到高于 `low` 的最后一格。
pub(crate) fn cell_prices(high: Price, low: Price, spacing: Price) -> Vec<Price> {
    let count = cell_count(high, low, spacing);
    (0..count)
        .map(|step| Price::from_units(high.units() - step * spacing.units()))
        .collect()
}

/// 规整后的网格形状：划分批次与格子时反复用到的参数。
pub(crate) struct GridShape {
    pub price_low: Price,
    pub price_high: Price,
    pub max_position: Quantity,
    pub core_ratio: Rate,
    pub grid_spread: Price,
}

/// 把 `M` 按数量最小单位平均分给各格，余下的最小单位依次给靠近空隙的格子。
pub(crate) fn build_cells(
    prices: &[Price],
    shape: &GridShape,
    spec: &InstrumentSpec,
) -> Result<Vec<Cell>, ConfigError> {
    let step = spec.quantity_step();
    prices
        .iter()
        .enumerate()
        .map(|(index, price)| {
            Ok(Cell {
                index: CellIndex(index),
                buy_price: *price,
                sell_price: sell_price(*price, CellIndex(index), shape)?,
                quantity: cell_quantity(index, prices.len(), shape.max_position, step),
                batch: None,
            })
        })
        .collect::<Result<Vec<Cell>, ConfigError>>()
}

/// 格子卖价 = 买价 + 价差；加出来超出可表示范围时是配置错误，不做饱和。
pub(crate) fn sell_price(
    buy_price: Price,
    cell_index: CellIndex,
    shape: &GridShape,
) -> Result<Price, ConfigError> {
    buy_price
        .checked_add(shape.grid_spread)
        .ok_or(ConfigError::SellPriceOutOfRange {
            cell_index,
            buy_price,
            grid_spread: shape.grid_spread,
        })
}

pub(crate) fn cell_quantity(
    index: usize,
    cell_count: usize,
    max_position: Quantity,
    step: Quantity,
) -> Quantity {
    let total_steps = max_position.units() / step.units();
    Quantity::from_units(split_steps(total_steps, index, cell_count) * step.units())
}

/// 格子数量低于最低订单金额时，这一格至少需要多少数量。
///
/// 只用于错误信息。算出的数量超出可表示范围时按上限取值：反正这个配置本来就不合法。
pub(crate) fn required_cell_quantity(price: Price, spec: &InstrumentSpec) -> Quantity {
    let step_units = spec.quantity_step().units() as i128;
    let minimum_raw = 3 * spec.min_order_notional().raw();
    let price_units = (price.units() as i128).max(1);
    let units = div_ceil(minimum_raw, price_units) + 2 * step_units;
    let units = div_ceil(units, step_units) * step_units;
    Quantity::from_units(i64::try_from(units).unwrap_or(i64::MAX))
}

pub(crate) fn meets_minimum_cell_notional(
    quantity: Quantity,
    price: Price,
    spec: &InstrumentSpec,
) -> bool {
    let Some(three_minimum) = spec.min_order_notional().checked_mul(3) else {
        return false;
    };
    let two_steps = Notional::of(spec.quantity_step(), price)
        .checked_mul(2)
        .unwrap_or(Notional::ZERO);
    Notional::of(quantity, price) >= three_minimum + two_steps
}

/// 同样的价格范围与间距下，最多能设多少格。
///
/// 格数越少，每格数量越大、最低价的格子也越高，所以最低金额约束随格数单调放松。
/// 最低价的一格总是最严格的一格，因此只看它。
pub(crate) fn max_cell_count(
    cell_count: usize,
    price_high: Price,
    max_position: Quantity,
    spec: &InstrumentSpec,
    grid_spacing: Price,
) -> usize {
    for candidate in (1..=cell_count).rev() {
        // 用 i128 算这一段的下限价：格数与间距都可能是极端值，i64 相乘会溢出。
        let offset = (candidate as i128 - 1) * grid_spacing.units() as i128;
        let price_units = (price_high.units() as i128 - offset).max(1);
        let price = Price::from_units(price_units as i64);
        let quantity = cell_quantity(candidate - 1, candidate, max_position, spec.quantity_step());
        if meets_minimum_cell_notional(quantity, price, spec) {
            return candidate;
        }
    }
    0
}

/// 双边都以 taker 成交是否仍然盈利：`(卖价 − 买价) > (卖价 + 买价) × taker 费率`。
pub(crate) fn spread_is_profitable(buy: Price, sell: Price, taker: Rate) -> bool {
    let profit = (sell.units() as i128 - buy.units() as i128) * SCALE as i128;
    let cost = (sell.units() as i128 + buy.units() as i128) * taker.units() as i128;
    profit > cost
}

/// 满足扣费后盈利的最小价差，向上取到一个报价单位。
///
/// `(卖价 − 买价) × SCALE > (卖价 + 买价) × taker` 等价于
/// `卖价 > 买价 × (SCALE + taker) / (SCALE − taker)`，所以最小卖价可以直接算出来，
/// 不需要逐个报价单位试探。
///
/// 两种算不出来的情况分开报：费率本身就不可能盈利，和这一格的卖价超出了可表示范围。
/// 两者的处理方式完全不同——前者要去改费率，后者要去改网格。
pub(crate) fn minimum_grid_spread(
    price: Price,
    taker: Rate,
    tick: Price,
) -> Result<Price, ConfigError> {
    let price_units = price.units() as i128;
    let taker_units = taker.units() as i128;
    let denominator = SCALE as i128 - taker_units;
    if denominator <= 0 {
        // taker 费率不小于 100%：任何价格上买入再卖出都亏。
        return Err(ConfigError::TakerFeeRateTooHigh {
            taker_fee_rate: taker,
        });
    }
    let numerator = price_units * (SCALE as i128 + taker_units);
    // 向下取整再加一，得到满足严格大于的最小整数卖价。
    let sell_units = numerator / denominator + 1;
    let tick_units = tick.units() as i128;
    let sell_units = div_ceil(sell_units, tick_units) * tick_units;
    let unreachable = || ConfigError::GridSpreadUnreachable {
        buy_price: price,
        taker_fee_rate: taker,
    };
    let sell_units = i64::try_from(sell_units).map_err(|_| unreachable())?;
    let spread = sell_units
        .checked_sub(price.units())
        .ok_or_else(unreachable)?;
    debug_assert!(spread_is_profitable(
        price,
        Price::from_units(sell_units),
        taker
    ));
    Ok(Price::from_units(spread))
}

#[cfg(test)]
mod tests {
    use super::{cell_prices, minimum_grid_spread, spread_is_profitable};
    use crate::error::ConfigError;
    use crate::units::{Price, Rate};

    #[test]
    fn cell_prices_start_at_high_and_stop_above_low() {
        let prices = cell_prices(
            Price::parse("95000").unwrap(),
            Price::parse("75000").unwrap(),
            Price::parse("1000").unwrap(),
        );

        assert_eq!(prices.len(), 20);
        assert_eq!(prices[0], Price::parse("95000").unwrap());
        assert_eq!(prices[19], Price::parse("76000").unwrap());
        assert!(
            prices
                .iter()
                .all(|price| *price > Price::parse("75000").unwrap())
        );
    }

    #[test]
    fn cell_prices_cover_a_single_cell_when_spacing_exceeds_range() {
        let prices = cell_prices(
            Price::parse("95000").unwrap(),
            Price::parse("75000").unwrap(),
            Price::parse("100000").unwrap(),
        );

        assert_eq!(prices, vec![Price::parse("95000").unwrap()]);
    }

    /// 两条腿都按 taker 收费：价差要盖过两份手续费才算盈利。
    #[test]
    fn a_spread_is_profitable_only_above_the_cost_of_both_legs() {
        let buy = Price::parse("100").unwrap();
        let taker = Rate::parse("0.00045").unwrap();

        // 100 → 100.1：价差 0.1，两份手续费约 0.09。
        assert!(spread_is_profitable(
            buy,
            Price::parse("100.1").unwrap(),
            taker
        ));
        // 100 → 100.05：价差盖不住手续费。
        assert!(!spread_is_profitable(
            buy,
            Price::parse("100.05").unwrap(),
            taker
        ));
    }

    #[test]
    fn the_minimum_spread_is_the_first_one_that_stays_profitable() {
        let buy = Price::parse("100").unwrap();
        let taker = Rate::parse("0.00045").unwrap();
        let tick = Price::parse("0.01").unwrap();

        let spread = minimum_grid_spread(buy, taker, tick).unwrap();

        assert!(spread_is_profitable(buy, buy.saturating_add(spread), taker));
        // 少一个报价单位就不盈利：这是能满足约束的最小价差，不是保守的估计值。
        assert!(!spread_is_profitable(
            buy,
            buy.saturating_add(spread).saturating_sub(tick),
            taker
        ));
    }

    /// taker 费率不小于 100% 时任何价格上买入再卖出都亏：这是费率的问题，不是网格的问题。
    #[test]
    fn a_taker_rate_of_one_hundred_percent_can_never_be_profitable() {
        let error = minimum_grid_spread(
            Price::parse("100").unwrap(),
            Rate::parse("1").unwrap(),
            Price::parse("0.01").unwrap(),
        );

        assert!(matches!(
            error,
            Err(ConfigError::TakerFeeRateTooHigh { .. })
        ));
    }
}
