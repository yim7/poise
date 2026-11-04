//! 批次划分（主仓）。
//!
//! 一侧把 `[L, H]` 均分为 `n` 段，段宽 `w = (H − L) / n`，第 k 批的批次价在启动
//! 顺序上依次相隔 `w`：多头侧从 `H` 往下，空头侧从 `L` 往上。数量有两种分法（第 3.1 节）：
//!
//! - 没有主仓倾斜：每批数量把 `M` 按数量最小单位平均分成 n 份，除不尽的余数依次多给
//!   靠近空隙的批次；每批的主仓 `C = ceil(r × q)`，网格部分 `G = q − C` 平均分给跟随
//!   本批的格子。
//! - 有主仓倾斜：主仓总量 `K = ceil(r × M)` 按权重分到各批，末批 ÷ 首批等于倾斜比，
//!   中间线性过渡；网格总量 `M − K` 平均分给本侧全部格子，和批次无关。每批数量
//!   `q = C + 本批格子数量之和`。
//!
//! 运行中的策略每一步都从配置重新划分，所以没设倾斜的配置必须保持原来的分法不变。
//!
//! 批次按下标排列，0 是最靠近空隙的一批，也是启动顺序里的第一个。
//!
//! 划分批次时同时把格子的数量定下来：没有倾斜时格子的数量由本批的主仓数量决定，
//! 两者不能分开算。

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::grid::{Cell, CellIndex, GridShape, cell_quantity, exit_price};
use crate::instrument::InstrumentSpec;
use crate::side::Direction;
use crate::units::{
    Price, Quantity, Rate, SCALE, div_ceil, div_round, split_steps, split_steps_by_weight,
};

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
    /// 跟随本批的格子下标，按启动顺序，也就是从靠近批次价的一端开始。
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

    /// 按启动顺序排列。
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

/// 第 k 批的批次价：从启动端的价格起，每隔 `(H − L) / n` 一批，按报价精度规整一次。
///
/// 段宽 `w = (H − L) / n` 除不尽时，把余数分摊到各段上，而不是全部留给最后一段。
/// 中间量保留整数分子分母（`(起点 × n ± k × (H − L)) / n`），只在最后规整一次：
/// 先把分段偏移取整、再按报价精度规整，会多做一次取整，把价格推到相邻的报价单位上，
/// 从而改变这个价格落在哪一批。
///
/// 规整后可能有两批落在同一个价格上，那种配置会被「每批都要有格子」的校验拒绝。
/// 规整结果超出可表示范围时返回 `None`。
pub(crate) fn batch_price(
    direction: Direction,
    low: Price,
    high: Price,
    count: u32,
    index: usize,
    tick: Price,
) -> Option<Price> {
    debug_assert!(count > 0, "batch count must be positive");
    debug_assert!(high.units() > low.units(), "the range must ascend");
    debug_assert!(index < count as usize, "the index must be inside the range");
    debug_assert!(tick.units() > 0, "price tick must be positive");
    let start = i128::from(direction.start(low, high).units());
    let span = i128::from(high.units()) - i128::from(low.units());
    let count = i128::from(count);
    let tick = i128::from(tick.units());
    // 分子恒为正：多头侧是 `H × n − k × span ≤ H × n`，空头侧是 `L × n + k × span ≤ H × n`。
    let numerator = start * count + i128::from(direction.offset_sign()) * span * index as i128;
    let steps = div_round(numerator, count * tick);
    i64::try_from(steps * tick).ok().map(Price::from_units)
}

/// 划分批次，并定下每个格子的数量。
///
/// `prices` 为空表示这一侧没有网格（`r = 1`）：每批只有主仓，没有格子，
/// 也就不适用「每批都要有格子」这条规则。
pub(crate) fn build_batches(
    direction: Direction,
    prices: &[Price],
    shape: &GridShape,
    batch_count: u32,
    spec: &InstrumentSpec,
) -> Result<(BatchPlan, Vec<Cell>), ConfigError> {
    let count = batch_count as usize;
    let batch_prices = batch_prices(direction, shape, batch_count, spec)?;
    let following = following_batches(direction, prices, &batch_prices);
    let members: Vec<Vec<CellIndex>> = (0..count)
        .map(|index| members_of(&following, index))
        .collect();
    let quantities = match shape.core_skew {
        None => split_by_batch(shape, &members, spec),
        Some(skew) => split_core_and_grid(shape, skew, &members, prices.len(), spec),
    };

    let mut batches = Vec::with_capacity(count);
    let mut cells = Vec::with_capacity(prices.len());
    for index in 0..count {
        check_batch_has_cells(
            direction,
            &members[index],
            prices,
            &batch_prices,
            index,
            shape,
        )?;
        let core_quantity = quantities.core[index];
        check_core_notional(
            direction,
            BatchIndex(index),
            core_quantity,
            batch_prices[index],
            spec,
        )?;
        cells.extend(batch_cells(
            direction,
            &members[index],
            &quantities.cells,
            prices,
            shape,
            spec,
            BatchIndex(index),
        )?);
        let grid_quantity = members[index].iter().fold(Quantity::ZERO, |total, cell| {
            total + quantities.cells[cell.0]
        });
        batches.push(Batch {
            index: BatchIndex(index),
            price: batch_prices[index],
            quantity: core_quantity + grid_quantity,
            core_quantity,
            grid_quantity,
            cells: members[index].clone(),
        });
    }

    Ok((BatchPlan::from_batches(batches), cells))
}

/// 数量划分的结果：每批的主仓数量，以及按格子下标排列的每格数量。
struct Quantities {
    core: Vec<Quantity>,
    cells: Vec<Quantity>,
}

/// 没有主仓倾斜：`M` 平均分给各批，每批的主仓 `C = ceil(r × q)`，网格部分 `q − C`
/// 平均分给跟随本批的格子，余下的最小单位依次给靠近批次价的格子。
fn split_by_batch(
    shape: &GridShape,
    members: &[Vec<CellIndex>],
    spec: &InstrumentSpec,
) -> Quantities {
    let step = spec.quantity_step();
    let batch_quantities = batch_quantities(shape, members.len(), step);
    let core: Vec<Quantity> = batch_quantities
        .iter()
        .map(|quantity| core_quantity(*quantity, shape.core_ratio, step))
        .collect();
    let mut cells = vec![Quantity::ZERO; members.iter().map(Vec::len).sum()];
    for (index, batch_members) in members.iter().enumerate() {
        let grid_steps = batch_quantities[index].saturating_sub(core[index]).units() / step.units();
        for (position, cell) in batch_members.iter().enumerate() {
            cells[cell.0] = Quantity::from_units(
                split_steps(grid_steps, position, batch_members.len()) * step.units(),
            );
        }
    }
    Quantities { core, cells }
}

/// 有主仓倾斜：主仓总量 `ceil(r × M)` 按倾斜权重分到各批，网格总量平均分给本侧全部
/// 格子，余下的最小单位依次给靠近空隙的格子（和没有主仓时格子的分法一样）。
fn split_core_and_grid(
    shape: &GridShape,
    skew: Rate,
    members: &[Vec<CellIndex>],
    cell_count: usize,
    spec: &InstrumentSpec,
) -> Quantities {
    let step = spec.quantity_step();
    let core_total = core_quantity(shape.max_position, shape.core_ratio, step);
    let core = split_steps_by_weight(
        core_total.units() / step.units(),
        &core_weights(members.len(), skew),
    )
    .into_iter()
    .map(|steps| Quantity::from_units(steps * step.units()))
    .collect();
    let grid_total = shape.max_position.saturating_sub(core_total);
    let cells = (0..cell_count)
        .map(|index| cell_quantity(index, cell_count, grid_total, step))
        .collect();
    Quantities { core, cells }
}

/// 主仓倾斜的权重：末批 ÷ 首批等于倾斜比，中间线性过渡。
///
/// 第 k 批的权重是 `(n − 1) + (倾斜比 − 1) × k`，按比率的最小单位放大后都是整数；
/// 倾斜比为正时每个权重都是正数（末批是 `(n − 1) × 倾斜比`）。只有一批时权重不起作用。
fn core_weights(count: usize, skew: Rate) -> Vec<i128> {
    if count == 1 {
        return vec![1];
    }
    let last = count as i128 - 1;
    (0..count as i128)
        .map(|index| {
            last * i128::from(SCALE) + (i128::from(skew.units()) - i128::from(SCALE)) * index
        })
        .collect()
}

/// 批次价：把范围均分后按报价精度规整。
///
/// 报价规则随价格变粗，批次价都在 `[L, H]` 里，按 `H` 处的步长规整，别处的步长只会
/// 更细，规整出来的价格处处都落得下。
fn batch_prices(
    direction: Direction,
    shape: &GridShape,
    batch_count: u32,
    spec: &InstrumentSpec,
) -> Result<Vec<Price>, ConfigError> {
    let tick = spec.price_rule().step_at(shape.price_high);
    (0..batch_count as usize)
        .map(|index| {
            batch_price(
                direction,
                shape.price_low,
                shape.price_high,
                batch_count,
                index,
                tick,
            )
            .ok_or(ConfigError::BatchPriceOutOfRange {
                direction,
                batch_index: BatchIndex(index),
                price_low: shape.price_low,
                price_high: shape.price_high,
            })
        })
        .collect()
}

/// 每批数量：`M` 按数量最小单位平均分成 `count` 份，余数依次多给靠近空隙的批次。
fn batch_quantities(shape: &GridShape, count: usize, step: Quantity) -> Vec<Quantity> {
    let total_steps = shape.max_position.units() / step.units();
    (0..count)
        .map(|index| Quantity::from_units(split_steps(total_steps, index, count) * step.units()))
        .collect()
}

/// 每个格子跟随价格上最先到达它的那一批，即价格到达这一格之前最后到达的那一批。
///
/// 兜底给第 0 批是承重的，不是不可达：空头侧第 0 批的批次价是按较粗的 `H` 端步长规整
/// 出来的，可能高于 `L`，最低那一格的建仓价就落在它下面，没有批次到达。这时最低的一格
/// 跟随第 0 批正是想要的结果。
fn following_batches(direction: Direction, prices: &[Price], batch_prices: &[Price]) -> Vec<usize> {
    prices
        .iter()
        .map(|price| {
            (0..batch_prices.len())
                .rev()
                .find(|index| direction.reached(batch_prices[*index], *price))
                .unwrap_or(0)
        })
        .collect()
}

/// 第 `index` 批的成员格子，按启动顺序。
fn members_of(following: &[usize], index: usize) -> Vec<CellIndex> {
    following
        .iter()
        .enumerate()
        .filter(|(_, batch)| **batch == index)
        .map(|(cell, _)| CellIndex(cell))
        .collect()
}

/// 每批都要有格子接手它的网格部分；这一侧没有网格（`r = 1`）时不适用。
fn check_batch_has_cells(
    direction: Direction,
    members: &[CellIndex],
    prices: &[Price],
    batch_prices: &[Price],
    index: usize,
    shape: &GridShape,
) -> Result<(), ConfigError> {
    if !members.is_empty() || prices.is_empty() {
        return Ok(());
    }
    // 这一批负责的价格段：到相邻那一批为止。两侧的批次价排列方向相反，所以按大小取
    // 上下限；最后一批的另一端就是这一侧远离空隙的那一端（多头侧 `L`、空头侧 `H`）。
    let neighbour = batch_prices
        .get(index + 1)
        .copied()
        .unwrap_or(match direction {
            Direction::Long => shape.price_low,
            Direction::Short => shape.price_high,
        });
    Err(ConfigError::BatchWithoutCell {
        direction,
        batch_index: BatchIndex(index),
        batch_price: batch_prices[index],
        segment_low: neighbour.min(batch_prices[index]),
        segment_high: neighbour.max(batch_prices[index]),
    })
}

/// 本批的主仓按批次价要顶得住最低订单金额。
fn check_core_notional(
    direction: Direction,
    index: BatchIndex,
    core_quantity: Quantity,
    batch_price: Price,
    spec: &InstrumentSpec,
) -> Result<(), ConfigError> {
    if spec.meets_minimum_order(core_quantity, batch_price) {
        return Ok(());
    }
    Err(ConfigError::BatchCoreBelowMinimumOrderNotional {
        direction,
        batch_index: index,
        batch_price,
        core_quantity,
        required_quantity: required_core_quantity(batch_price, spec),
    })
}

/// 本批的格子，数量取划分好的 `cell_quantities`。
///
/// `members` 按启动顺序排列，也就是从靠近批次价的一端开始。
fn batch_cells(
    direction: Direction,
    members: &[CellIndex],
    cell_quantities: &[Quantity],
    prices: &[Price],
    shape: &GridShape,
    spec: &InstrumentSpec,
    batch: BatchIndex,
) -> Result<Vec<Cell>, ConfigError> {
    members
        .iter()
        .map(|cell_index| {
            let entry_price = prices[cell_index.0];
            Ok(Cell {
                index: *cell_index,
                entry_price,
                exit_price: exit_price(direction, entry_price, *cell_index, shape, spec)?,
                quantity: cell_quantities[cell_index.0],
                batch: Some(batch),
            })
        })
        .collect()
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
    use super::{batch_price, core_quantity, core_weights};
    use crate::side::Direction;
    use crate::units::{Price, Quantity, Rate};

    /// 报价精度为一个报价单位，也就是常见的整数报价。
    fn unit_tick() -> Price {
        Price::parse("1").unwrap()
    }

    fn price(high: Price, low: Price, count: u32, index: usize) -> Price {
        batch_price(Direction::Long, low, high, count, index, unit_tick()).unwrap()
    }

    fn short_price(low: Price, high: Price, count: u32, index: usize) -> Price {
        batch_price(Direction::Short, low, high, count, index, unit_tick()).unwrap()
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

    /// 空头侧的批次价是多头侧的镜像：从 `L` 往上，同一批的价格关于 `L + H` 对称。
    #[test]
    fn b10_short_batch_prices_start_at_low_and_mirror_the_long_side() {
        let high = Price::parse("95000").unwrap();
        let low = Price::parse("75000").unwrap();

        assert_eq!(short_price(low, high, 4, 0), low);
        assert_eq!(short_price(low, high, 4, 1), Price::parse("80000").unwrap());
        assert_eq!(short_price(low, high, 4, 2), Price::parse("85000").unwrap());
        assert_eq!(short_price(low, high, 4, 3), Price::parse("90000").unwrap());
        assert!(short_price(low, high, 4, 3) < high);
        for index in 0..4 {
            assert_eq!(
                price(high, low, 4, index).units() + short_price(low, high, 4, index).units(),
                low.units() + high.units(),
                "第 {index} 批不是镜像"
            );
        }
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
            batch_price(Direction::Long, low, high, 5, 1, tick).unwrap(),
            Price::parse("174.99999998").unwrap()
        );
        assert!(
            batch_price(Direction::Long, low, high, 5, 1, tick).unwrap()
                < Price::parse("175").unwrap()
        );
    }

    /// 规整结果超出可表示范围时返回 `None`，不 panic 也不饱和。
    #[test]
    fn reports_batch_prices_that_are_not_representable() {
        // 第 0 批的批次价是 `H`，按一个报价单位规整后落到 `92233720369`，
        // 它的最小单位超过 `i64::MAX`。
        let high = Price::parse("92233720368.54775807").unwrap();
        let low = Price::parse("92233720367").unwrap();

        assert_eq!(
            batch_price(Direction::Long, low, high, 2, 0, unit_tick()),
            None
        );
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

    /// 倾斜权重：末批 ÷ 首批等于倾斜比，中间线性过渡；按比率最小单位放大后是整数。
    #[test]
    fn core_weights_run_linearly_from_one_to_the_skew() {
        let scale = i128::from(crate::units::SCALE);
        // 倾斜 3、四批：(n − 1) + (3 − 1) × k = 3、5、7、9。
        assert_eq!(
            core_weights(4, Rate::parse("3").unwrap()),
            vec![3 * scale, 5 * scale, 7 * scale, 9 * scale]
        );
        // 倾斜 .5、三批：2、1.5、1，末批是首批的一半。
        assert_eq!(
            core_weights(3, Rate::parse("0.5").unwrap()),
            vec![2 * scale, 3 * scale / 2, scale]
        );
        assert_eq!(core_weights(1, Rate::parse("3").unwrap()), vec![1]);
        assert!(
            core_weights(10_000, Rate::parse("0.01").unwrap())
                .iter()
                .all(|weight| *weight > 0)
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
