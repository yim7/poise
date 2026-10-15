//! 批次划分（主仓）。
//!
//! 多头侧把 `[L, H]` 均分为 `n` 段，段宽 `w = (H − L) / n`，第 k 批的批次价是
//! `H − k × w`，对应的段是 `(H − (k+1)w, H − k × w]`。每批数量把 `M` 按数量
//! 最小单位平均分成 n 份，除不尽的余数依次多给靠近空隙的批次；每批的主仓
//! `C = ceil(r × q)`，网格部分 `G = q − C`。
//!
//! 批次按下标排列，0 是最靠近空隙（价格最高）的一批，也是启动顺序里的第一个。
//!
//! 划分批次时同时把每批的网格部分分给跟随它的格子——格子的数量由本批的主仓数量
//! 决定，两者不能分开算。

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::grid::{Cell, CellIndex, GridShape, sell_price};
use crate::instrument::InstrumentSpec;
use crate::units::{Price, Quantity, Rate, div_ceil, div_round, split_steps};

/// 批次在划分中的下标，0 是最靠近空隙的一批。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BatchIndex(pub usize);

impl std::fmt::Display for BatchIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// 一批主仓：批次价、总量、主仓部分和跟随它的格子。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub index: BatchIndex,
    /// 批次价，本批主仓买单挂在这里，不追高。
    pub price: Price,
    /// 本批总量 `q`。
    pub quantity: Quantity,
    /// 主仓部分 `C = ceil(r × q)`，按数量最小单位向上取整。
    pub core_quantity: Quantity,
    /// 网格部分 `G = q − C`，平均分给跟随本批的格子。
    pub grid_quantity: Quantity,
    /// 跟随本批的格子下标，按价格从高到低，也就是从靠近批次价的一端开始。
    pub cells: Vec<CellIndex>,
}

/// 一个策略的批次划分。主仓比例为 0 时没有批次。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchPlan {
    batches: Vec<Batch>,
}

impl BatchPlan {
    pub(crate) fn from_batches(batches: Vec<Batch>) -> Self {
        Self { batches }
    }

    /// 按价格从高到低排列。
    pub fn batches(&self) -> &[Batch] {
        &self.batches
    }

    pub fn len(&self) -> usize {
        self.batches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    pub fn batch(&self, index: BatchIndex) -> Option<&Batch> {
        self.batches.get(index.0)
    }
}

/// 第 k 批的批次价：`H − k × (H − L) / n`，按报价精度规整一次。
///
/// 段宽 `w = (H − L) / n` 除不尽时，把余数分摊到各段上，而不是全部留给最后一段。
/// 中间量保留整数分子分母（`(H × n − k × (H − L)) / n`），只在最后规整一次：
/// 先把分段偏移取整、再按报价精度规整，会多做一次取整，把价格推到相邻的报价单位上，
/// 从而改变这个价格落在哪一批。
///
/// 规整后可能有两批落在同一个价格上，那种配置会被「每批都要有格子」的校验拒绝。
/// 规整结果超出可表示范围时返回 `None`。
pub(crate) fn batch_price(
    high: Price,
    low: Price,
    count: u32,
    index: usize,
    tick: Price,
) -> Option<Price> {
    debug_assert!(count > 0, "batch count must be positive");
    debug_assert!(high.units() > low.units(), "the range must ascend");
    debug_assert!(index < count as usize, "the index must be inside the range");
    debug_assert!(tick.units() > 0, "price tick must be positive");
    let high_units = i128::from(high.units());
    let span = high_units - i128::from(low.units());
    let count = i128::from(count);
    let tick = i128::from(tick.units());
    // 分子恒为正：`span ≤ H`，所以 `H × n − k × span ≥ H`。
    let numerator = high_units * count - span * index as i128;
    let steps = div_round(numerator, count * tick);
    i64::try_from(steps * tick).ok().map(Price::from_units)
}

/// 划分批次，并把每批的网格部分分给跟随它的格子。
///
/// `prices` 为空表示这一侧没有网格（`r = 1`）：每批只有主仓，没有格子，
/// 也就不适用「每批都要有格子」这条规则。
pub(crate) fn build_batches(
    prices: &[Price],
    shape: &GridShape,
    batch_count: u32,
    spec: &InstrumentSpec,
) -> Result<(BatchPlan, Vec<Cell>), ConfigError> {
    let count = batch_count as usize;
    let step = spec.quantity_step();
    let (price_high, price_low) = (shape.price_high, shape.price_low);

    // 批次价：把范围均分后按报价精度规整。报价规则随价格变粗，批次价都在 `H` 以下，
    // 按 `H` 处的步长规整，越往下步长只会更细，规整出来的价格处处都落得下。
    let tick = spec.price_rule().step_at(price_high);
    let batch_prices: Vec<Price> = (0..count)
        .map(|index| {
            batch_price(price_high, price_low, batch_count, index, tick).ok_or(
                ConfigError::BatchPriceOutOfRange {
                    batch_index: BatchIndex(index),
                    price_low,
                    price_high,
                },
            )
        })
        .collect::<Result<Vec<Price>, ConfigError>>()?;

    // 每批数量：M 按数量最小单位平均分成 n 份，余数依次多给靠近空隙的批次。
    let total_steps = shape.max_position.units() / step.units();
    let quantities: Vec<Quantity> = (0..count)
        .map(|index| Quantity::from_units(split_steps(total_steps, index, count) * step.units()))
        .collect();

    // 每个格子跟随价格上最先到达它的那一批，即批次价里不小于格子价的最低那一批。
    // 第 0 批的批次价就是 H，所以一定找得到。
    let following: Vec<usize> = prices
        .iter()
        .map(|price| {
            (0..count)
                .rev()
                .find(|index| batch_prices[*index] >= *price)
                .unwrap_or(0)
        })
        .collect();

    let mut batches = Vec::with_capacity(count);
    let mut cells = Vec::with_capacity(prices.len());
    for index in 0..count {
        let batch_index = BatchIndex(index);
        let cells_of_batch: Vec<CellIndex> = following
            .iter()
            .enumerate()
            .filter(|(_, batch)| **batch == index)
            .map(|(cell, _)| CellIndex(cell))
            .collect();
        if cells_of_batch.is_empty() && !prices.is_empty() {
            return Err(ConfigError::BatchWithoutCell {
                batch_index,
                batch_price: batch_prices[index],
                segment_low: batch_prices.get(index + 1).copied().unwrap_or(price_low),
                segment_high: batch_prices[index],
            });
        }

        let core_quantity = core_quantity(quantities[index], shape.core_ratio, step);
        if !spec.meets_minimum_order(core_quantity, batch_prices[index]) {
            return Err(ConfigError::BatchCoreBelowMinimumOrderNotional {
                batch_index,
                batch_price: batch_prices[index],
                core_quantity,
                required_quantity: required_core_quantity(batch_prices[index], spec),
            });
        }
        let grid_quantity = quantities[index].saturating_sub(core_quantity);

        // 网格部分平均分给跟随本批的格子，余下的最小单位依次分给靠近批次价的格子。
        // `cells_of_batch` 按价格从高到低排列，也就是从靠近批次价的一端开始。
        let grid_steps = grid_quantity.units() / step.units();
        for (position, cell_index) in cells_of_batch.iter().enumerate() {
            let quantity = Quantity::from_units(
                split_steps(grid_steps, position, cells_of_batch.len()) * step.units(),
            );
            cells.push(Cell {
                index: *cell_index,
                buy_price: prices[cell_index.0],
                sell_price: sell_price(prices[cell_index.0], *cell_index, shape, spec)?,
                quantity,
                batch: Some(batch_index),
            });
        }

        batches.push(Batch {
            index: batch_index,
            price: batch_prices[index],
            quantity: quantities[index],
            core_quantity,
            grid_quantity,
            cells: cells_of_batch,
        });
    }

    Ok((BatchPlan::from_batches(batches), cells))
}

/// 一批的主仓数量 `C = ceil(r × q)`，按数量最小单位向上取整。
///
/// `quantity` 本身是数量精度的整数倍，所以向上规整的结果不会超过它，
/// 也就不会超出可表示范围；真越界了也夹回本批数量。
pub(crate) fn core_quantity(quantity: Quantity, core_ratio: Rate, step: Quantity) -> Quantity {
    quantity
        .mul_rate_ceil(core_ratio)
        .checked_ceil_to_step(step)
        .unwrap_or(quantity)
        .min(quantity)
}

/// 一批主仓买入低于最低订单金额时，这一批至少需要多少数量。
///
/// 只用于错误信息。算出的数量超出可表示范围时按上限取值：反正这个配置本来就不合法。
pub(crate) fn required_core_quantity(price: Price, spec: &InstrumentSpec) -> Quantity {
    let step_units = spec.quantity_step().units() as i128;
    let price_units = (price.units() as i128).max(1);
    let units = div_ceil(spec.min_order_notional().raw(), price_units);
    let units = div_ceil(units, step_units) * step_units;
    Quantity::from_units(i64::try_from(units).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{batch_price, core_quantity};
    use crate::units::{Price, Quantity, Rate};

    /// 报价精度为一个报价单位，也就是常见的整数报价。
    fn unit_tick() -> Price {
        Price::parse("1").unwrap()
    }

    fn price(high: Price, low: Price, count: u32, index: usize) -> Price {
        batch_price(high, low, count, index, unit_tick()).unwrap()
    }

    #[test]
    fn batch_prices_split_the_range_into_equal_segments() {
        let high = Price::parse("95000").unwrap();
        let low = Price::parse("75000").unwrap();

        assert_eq!(price(high, low, 4, 0), high);
        assert_eq!(price(high, low, 4, 1), Price::parse("90000").unwrap());
        assert_eq!(price(high, low, 4, 2), Price::parse("85000").unwrap());
        assert_eq!(price(high, low, 4, 3), Price::parse("80000").unwrap());
        assert!(price(high, low, 4, 3) > low);
    }

    #[test]
    fn batch_prices_share_the_remainder_instead_of_giving_it_to_the_last_batch() {
        let high = Price::parse("95000").unwrap();
        let low = Price::parse("75000").unwrap();

        // 20000 / 3 除不尽：段宽在 6666 和 6667 之间分摊，最后一批仍在范围内。
        assert_eq!(price(high, low, 3, 1), Price::parse("88333").unwrap());
        assert_eq!(price(high, low, 3, 2), Price::parse("81667").unwrap());
        assert!(price(high, low, 3, 2) > low);
        // 余数全部留给最后一段的话，最后一批会落到 81667 以下。
        assert!(price(high, low, 3, 2) > Price::parse("81666").unwrap());
    }

    /// 精确批次价是 `174.999999988`，报价精度 `0.00000002` 下应当规整成
    /// `174.99999998`。先把偏移取整到 `1e-8` 得到 `174.99999999`，再规整就成了
    /// `175`：多做一次取整会把批次价推高一整个报价单位。
    #[test]
    fn a_batch_price_is_rounded_to_the_tick_only_once() {
        let high = Price::parse("200").unwrap();
        let low = Price::parse("74.99999994").unwrap();
        let tick = Price::parse("0.00000002").unwrap();

        assert_eq!(
            batch_price(high, low, 5, 1, tick).unwrap(),
            Price::parse("174.99999998").unwrap()
        );
        assert!(batch_price(high, low, 5, 1, tick).unwrap() < Price::parse("175").unwrap());
    }

    /// 规整结果超出可表示范围时返回 `None`，不 panic 也不饱和。
    #[test]
    fn reports_batch_prices_that_are_not_representable() {
        // 第 0 批的批次价是 `H`，按一个报价单位规整后落到 `92233720369`，
        // 它的最小单位超过 `i64::MAX`。
        let high = Price::parse("92233720368.54775807").unwrap();
        let low = Price::parse("92233720367").unwrap();

        assert_eq!(batch_price(high, low, 2, 0, unit_tick()), None);
    }

    /// 主仓数量按数量最小单位向上取整：`C = ceil(r × q)`，最多不超过本批总量 `q`。
    #[test]
    fn the_core_quantity_rounds_up_to_the_quantity_step_and_never_exceeds_the_batch() {
        let step = Quantity::parse("0.001").unwrap();
        let quantity = Quantity::parse("0.25").unwrap();

        // .7 × .25 = .175，正好是精度整数倍。
        assert_eq!(
            core_quantity(quantity, Rate::parse("0.7").unwrap(), step),
            Quantity::parse("0.175").unwrap()
        );
        // .3 × .25 = .075，同样正好。
        assert_eq!(
            core_quantity(quantity, Rate::parse("0.3").unwrap(), step),
            Quantity::parse("0.075").unwrap()
        );
        // 除不尽时向上取整，宁可多买主仓，也不要主仓买入金额不足。
        assert_eq!(
            core_quantity(quantity, Rate::parse("1").unwrap(), step),
            quantity
        );
        // r 按 1e-8 存，`.33333333 × .25` 向上取整到 .001 精度是 .084。
        assert_eq!(
            core_quantity(quantity, Rate::parse("0.33333333").unwrap(), step),
            Quantity::parse("0.084").unwrap()
        );
    }

    #[test]
    fn a_zero_core_ratio_has_no_core_quantity() {
        assert_eq!(
            core_quantity(
                Quantity::parse("0.25").unwrap(),
                Rate::ZERO,
                Quantity::parse("0.001").unwrap()
            ),
            Quantity::ZERO
        );
    }
}
