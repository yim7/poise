//! 格子划分。
//!
//! 一侧的格子建仓价从靠近空隙的一端开始，每隔 `d` 一格，直到范围内的最后一格：
//! 多头侧从最高价 `H` 往下，空头侧从最低价 `L` 往上。每一格再按它自己所在价位的
//! 报价精度规整（见 [`cell_prices`]），所以空头侧跨进更粗的档位时价格会略低于理想
//! 位置。每格的平仓价在建仓价的盈利一侧（多头侧 `+ g`，空头侧 `− g`）。格子按下标
//! 排列，0 是最靠近空隙的一格，也是启动顺序里的第一个。
//!
//! 格子数量、最低订单金额和价差盈利性都算在生成这一侧：它们决定了哪些配置根本
//! 挂不出单，属于划分本身的性质，不是配置字段的格式问题。

use serde::{Deserialize, Serialize};

use crate::batch::{BatchIndex, core_quantity};
use crate::error::ConfigError;
use crate::instrument::InstrumentSpec;
use crate::side::Direction;
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
    /// 建仓价：多头侧的格子买价，空头侧的开空价。
    pub entry_price: Price,
    /// 平仓价：多头侧的卖价 = 建仓价 + `g`，空头侧的买平价 = 建仓价 − `g`。
    pub exit_price: Price,
    /// 本格的网格数量：所在批次的网格部分平均分给跟随它的格子。
    pub quantity: Quantity,
    /// 跟随哪一批；主仓比例为 0 时没有批次，格子自成单位。
    pub batch: Option<BatchIndex>,
}

impl Cell {
    /// 两条腿里较低的那个价格。
    ///
    /// 最低订单金额按它算（设计第 2.3 节）：格子被部分成交拆成持有和已卖出两部分时，
    /// 数量少的那一部分要能挂得出去。
    pub fn lower_price(&self) -> Price {
        self.entry_price.min(self.exit_price)
    }
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

    /// 按启动顺序排列：多头侧从高到低，空头侧从低到高。
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

/// 从靠近空隙的一端开始每隔 `spacing` 一格，直到范围内的最后一格。
///
/// 多头侧是 `H`、`H − d` … 直到高于 `L` 的最后一格；空头侧是 `L`、`L + d` … 直到
/// 低于 `H` 的最后一格。两者格数相同；报价步长处处相同时价格按 `L + H` 对称。
///
/// 每一格的建仓价还要按**它自己所在价位**的报价精度规整（[`Direction::entry_rounding`]）：
/// 步长随价格变粗，空头侧从 `L` 往上排时会跨进更粗的档位，`L + k × d` 在那个档位上不
/// 一定是合法报价（5 位有效数字、步长 0.0001 时，`10.0099` 不是 10 那一档的 `0.001`
/// 的整数倍），挂出去交易所会拒掉。多头侧不受影响：`H` 和 `d` 都是 `H` 处步长的整数倍，
/// 更低价位处的步长整除它，所以每一格本来就落在自己的步长上，规整是空操作。
///
/// 规整结果超出可表示范围时是配置错误，不做饱和。
pub(crate) fn cell_prices(
    direction: Direction,
    low: Price,
    high: Price,
    spacing: Price,
    spec: &InstrumentSpec,
) -> Result<Vec<Price>, ConfigError> {
    let count = cell_count(high, low, spacing);
    let start = direction.start(low, high);
    (0..count)
        .map(|step| direction.offset_price(start, step * spacing.units()))
        .map(|price| align_entry_price(direction, price, spec))
        .collect()
}

/// 建仓价按它自己所在价位的报价精度规整，方向朝启动端。
///
/// 朝启动端规整的价格不会越过 `[L, H]`：空头侧向下规整的结果不超过原值，多头侧向上
/// 规整的结果不超过 `H`（`H` 本身就在步长上）。空头侧向下规整也一定落在可表示范围内。
fn align_entry_price(
    direction: Direction,
    price: Price,
    spec: &InstrumentSpec,
) -> Result<Price, ConfigError> {
    let tick = spec.price_rule().step_at(price);
    price
        .checked_round_to_tick(tick, direction.entry_rounding())
        .ok_or(ConfigError::PriceOutOfRange {
            direction,
            field: "格子价",
            price,
            price_tick: tick,
        })
}

/// 规整后的网格形状：划分批次与格子时反复用到的参数。
pub(crate) struct GridShape {
    pub price_low: Price,
    pub price_high: Price,
    pub max_position: Quantity,
    pub core_ratio: Rate,
    /// 主仓倾斜（末批 ÷ 首批）；`None` 是原来的分法：`M` 平均分给各批，网格随批。
    pub core_skew: Option<Rate>,
    pub grid_spacing: Price,
    pub grid_spread: Price,
}

/// 把 `M` 按数量最小单位平均分给各格，余下的最小单位依次给靠近空隙的格子。
pub(crate) fn build_cells(
    direction: Direction,
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
                // 建仓价在 [`cell_prices`] 里已经按各自价位的步长规整过，这里直接用。
                entry_price: *price,
                exit_price: exit_price(direction, *price, CellIndex(index), shape, spec)?,
                quantity: cell_quantity(index, prices.len(), shape.max_position, step),
                batch: None,
            })
        })
        .collect::<Result<Vec<Cell>, ConfigError>>()
}

/// 格子平仓价 = 建仓价 ∓ 价差，再按平仓价处的报价精度规整。
///
/// 平仓价在建仓价的盈利一侧，可能跨到步长更粗的价位上（例如 `0.099999` 上面就是
/// `0.1`）：按「让价差变大」的方向规整（多头侧向上、空头侧向下），价差只会变大，
/// 不会变小。空头侧减出来的价格可能减到零或负数，那样的平仓价挂不出去，按配置错误
/// 处理；加出来或规整后超出可表示范围时同样是配置错误，不做饱和。
pub(crate) fn exit_price(
    direction: Direction,
    entry_price: Price,
    cell_index: CellIndex,
    shape: &GridShape,
    spec: &InstrumentSpec,
) -> Result<Price, ConfigError> {
    let out_of_range = || ConfigError::ExitPriceOutOfRange {
        direction,
        cell_index,
        entry_price,
        grid_spread: shape.grid_spread,
    };
    let exit_price = direction
        .exit_price(entry_price, shape.grid_spread)
        .ok_or_else(out_of_range)?;
    if exit_price.units() <= 0 {
        return Err(out_of_range());
    }
    let exit_price = spec
        .normalize_price(exit_price, direction.exit_rounding())
        .ok_or_else(out_of_range)?;
    // 向下规整可能把平仓价压到零：零价的订单挂不出去。
    if exit_price.units() <= 0 {
        return Err(out_of_range());
    }
    Ok(exit_price)
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
/// 按网格总量平均分给各格估算。有主仓批次时格子按批分配，各批格数不一定相同，实际放得下
/// 的格数可能略少，所以报错里写「约」。
///
/// 格数越少，每格数量越大，被砍掉的又是靠近空隙那一端里最低金额最紧的格子，所以判据
/// 随格数单调放松，可以二分。逐格检查而不是只看最低价那一格：两条腿里较低价格最低的
/// 那一格（多头侧是最后一格，空头侧是第一格）不一定是数量最小的那一格，只看它得到的
/// 只是所有格子的下界。
pub(crate) fn max_cell_count(
    direction: Direction,
    cell_count: usize,
    shape: &GridShape,
    spec: &InstrumentSpec,
) -> usize {
    // `fits` 始终是放得下的格数，`fails` 始终是放不下的格数；0 格没有金额约束。
    let (mut fits, mut fails) = (0, cell_count + 1);
    while fails - fits > 1 {
        let candidate = (fits + fails) / 2;
        if cells_meet_minimum_notional(direction, candidate, shape, spec) {
            fits = candidate;
        } else {
            fails = candidate;
        }
    }
    fits
}

/// 把 `M` 分成 `count` 格时，每一格的金额都够最低下单金额。
///
/// 格子的建仓价只跟间距和起点有关，跟格数无关，所以取前 `count` 个价格就是分成 `count`
/// 格的样子。
fn cells_meet_minimum_notional(
    direction: Direction,
    count: usize,
    shape: &GridShape,
    spec: &InstrumentSpec,
) -> bool {
    let step = spec.quantity_step();
    // 有主仓时格子只分到网格那部分：M 减去主仓（第 4.2 节）。按整份 M 算会报出偏大的格数。
    let grid_total = shape.max_position.saturating_sub(core_quantity(
        shape.max_position,
        shape.core_ratio,
        step,
    ));
    let Ok(prices) = cell_prices(
        direction,
        shape.price_low,
        shape.price_high,
        shape.grid_spacing,
        spec,
    ) else {
        // 建仓价算不出来的配置本来就会以别的错误被拒，这里当作放不下。
        return false;
    };
    prices.iter().take(count).enumerate().all(|(index, entry)| {
        // 平仓价算不出来的配置本来就会以别的错误被拒，这里当作放不下。
        let Ok(exit) = exit_price(direction, *entry, CellIndex(index), shape, spec) else {
            return false;
        };
        let quantity = cell_quantity(index, count, grid_total, step);
        meets_minimum_cell_notional(quantity, (*entry).min(exit), spec)
    })
}

/// 两条腿都以 taker 成交是否仍然盈利：`|平仓价 − 建仓价| > (平仓价 + 建仓价) × taker 费率`。
///
/// 平仓价一定在建仓价的盈利一侧（多头侧在上、空头侧在下），所以差值按方向取正。
pub(crate) fn spread_is_profitable(
    direction: Direction,
    entry: Price,
    exit: Price,
    taker: Rate,
) -> bool {
    let difference = (exit.units() as i128 - entry.units() as i128) * direction.sign() as i128;
    let profit = difference * SCALE as i128;
    let cost = (exit.units() as i128 + entry.units() as i128) * taker.units() as i128;
    profit > cost
}

/// 满足扣费后盈利的最小价差，按「让价差变大」的方向取到一个报价单位。
///
/// 盈亏平衡价是 `建仓价 × (SCALE + sign × taker) / (SCALE − sign × taker)`，`sign` 多头
/// 为 +1、空头为 −1：多头侧的平仓价要严格高于它，空头侧要严格低于它。平衡价可以直接
/// 算出来，不需要逐个报价单位试探。两侧的价差大小一样，只有加减方向不同。
///
/// 两种算不出来的情况分开报：费率本身就不可能盈利，和这一格的平仓价超出了可表示范围。
/// 两者的处理方式完全不同——前者要去改费率，后者要去改网格。
pub(crate) fn minimum_grid_spread(
    direction: Direction,
    entry_price: Price,
    taker: Rate,
    tick: Price,
) -> Result<Price, ConfigError> {
    let sign = direction.sign() as i128;
    let price_units = entry_price.units() as i128;
    let taker_units = taker.units() as i128;
    if taker_units >= SCALE as i128 {
        // taker 费率不小于 100%：任何价格上建仓再平仓都亏。
        return Err(ConfigError::TakerFeeRateTooHigh {
            taker_fee_rate: taker,
        });
    }
    let unreachable = || ConfigError::GridSpreadUnreachable {
        direction,
        entry_price,
        taker_fee_rate: taker,
    };
    // 平衡价的分母两侧都是正的：上面的检查已经挡住了非正的 taker 余量。
    let (numerator, denominator) = match direction {
        Direction::Long => (
            price_units * (SCALE as i128 + taker_units),
            SCALE as i128 - taker_units,
        ),
        Direction::Short => (
            price_units * (SCALE as i128 - taker_units),
            SCALE as i128 + taker_units,
        ),
    };
    // 严格越过平衡价的那一个整数价格：多头向下取整再加一，空头向上取整再减一
    // （`(n − 1) / d` 对正数就是向上取整减一）。
    let exit_units = match direction {
        Direction::Long => numerator / denominator + 1,
        Direction::Short => (numerator - 1) / denominator,
    };
    let exit_units = i64::try_from(exit_units).map_err(|_| unreachable())?;
    // 规整到报价单位时继续朝盈利方向走：多头向上、空头向下，价差只会变大。
    let exit = Price::from_units(exit_units)
        .checked_round_to_tick(tick, direction.exit_rounding())
        .ok_or_else(unreachable)?;
    let spread = i64::try_from((exit.units() as i128 - price_units) * sign)
        .ok()
        .filter(|spread| *spread > 0)
        .ok_or_else(unreachable)?;
    debug_assert!(spread_is_profitable(direction, entry_price, exit, taker));
    Ok(Price::from_units(spread))
}

#[cfg(test)]
mod tests {
    use super::{cell_prices, minimum_grid_spread, spread_is_profitable};
    use crate::error::ConfigError;
    use crate::fixtures::{btc_spec, significant_figures};
    use crate::instrument::{Instrument, InstrumentSpec, PriceRule};
    use crate::side::Direction;
    use crate::units::{Notional, Price, Quantity, Rate};

    /// 测试里大多用固定报价步长的合约规则：规整是空操作，价格就是理想位置。
    fn prices(direction: Direction, low: &str, high: &str, spacing: &str) -> Vec<Price> {
        cell_prices(
            direction,
            Price::parse(low).unwrap(),
            Price::parse(high).unwrap(),
            Price::parse(spacing).unwrap(),
            &btc_spec(),
        )
        .unwrap()
    }

    #[test]
    fn cell_prices_start_at_high_and_stop_above_low() {
        let prices = prices(Direction::Long, "75000", "95000", "1000");

        assert_eq!(prices.len(), 20);
        assert_eq!(prices[0], Price::parse("95000").unwrap());
        assert_eq!(prices[19], Price::parse("76000").unwrap());
        assert!(
            prices
                .iter()
                .all(|price| *price > Price::parse("75000").unwrap())
        );
    }

    /// 空头侧是多头侧的镜像：从最低价往上，格数一样，价格关于 `L + H` 对称。
    #[test]
    fn b10_short_cell_prices_start_at_low_and_mirror_the_long_side() {
        let (low, high) = (
            Price::parse("75000").unwrap(),
            Price::parse("95000").unwrap(),
        );

        let long = prices(Direction::Long, "75000", "95000", "1000");
        let short = prices(Direction::Short, "75000", "95000", "1000");

        assert_eq!(short.len(), long.len());
        assert_eq!(short[0], Price::parse("75000").unwrap());
        assert_eq!(short[19], Price::parse("94000").unwrap());
        assert!(
            short
                .iter()
                .all(|price| *price < Price::parse("95000").unwrap())
        );
        // 对称：多头侧第 k 格的价格是 `L + H − 空头侧第 k 格的价格`。
        for (long, short) in long.iter().zip(short.iter()) {
            assert_eq!(
                long.units() + short.units(),
                low.units() + high.units(),
                "第 {short} 格不是 {long} 的镜像"
            );
        }
    }

    #[test]
    fn cell_prices_cover_a_single_cell_when_spacing_exceeds_range() {
        let prices = prices(Direction::Long, "75000", "95000", "100000");

        assert_eq!(prices, vec![Price::parse("95000").unwrap()]);
    }

    /// 空头侧从 `L` 往上排，跨进更粗的档位时 `L + k × d` 不是合法报价，逐格规整之后
    /// 才挂得出去。评审给的参数：5 位有效数字、步长下限 0.0001、`[9.9999, 10.1]`、
    /// `d = .01`；第二格的理想位置 10.0099 在 10 那一档上不是 `0.001` 的整数倍。
    #[test]
    fn b10_the_short_cell_prices_crossing_a_decade_land_on_their_own_tick() {
        let spec = InstrumentSpec::with_price_rule(
            Instrument::try_new("BTC").unwrap(),
            PriceRule::SignificantFigures {
                max_figures: 5,
                step_floor: Price::parse("0.0001").unwrap(),
            },
            Quantity::parse("0.01").unwrap(),
            Notional::parse("10").unwrap(),
        )
        .unwrap();

        let short = cell_prices(
            Direction::Short,
            Price::parse("9.9999").unwrap(),
            Price::parse("10.1").unwrap(),
            Price::parse("0.01").unwrap(),
            &spec,
        )
        .unwrap();

        assert_eq!(short.len(), 11);
        assert_eq!(short[0], Price::parse("9.9999").unwrap());
        // 跨过 10 之后按 10 那一档的 0.001 向下规整：10.0099 → 10.0090。
        assert_eq!(short[1], Price::parse("10.0090").unwrap());
        assert_eq!(short[10], Price::parse("10.0990").unwrap());
        for price in &short {
            let tick = spec.price_rule().step_at(*price);
            assert_eq!(
                price.units() % tick.units(),
                0,
                "{price} 不是它那一档的步长 {tick} 的整数倍"
            );
            assert!(
                significant_figures(*price) <= 5,
                "{price} 超过 5 位有效数字"
            );
            assert!(*price >= Price::parse("9.9999").unwrap());
            assert!(*price <= Price::parse("10.1").unwrap());
        }
        assert!(
            short.windows(2).all(|pair| pair[0] < pair[1]),
            "规整之后价格仍然要严格递增"
        );
    }

    /// 多头侧的建仓价本来就落在各自价位步长上：逐格规整不改变任何一格。
    #[test]
    fn b10_the_long_cell_prices_are_unchanged_by_aligning_them() {
        let spec = InstrumentSpec::with_price_rule(
            Instrument::try_new("BTC").unwrap(),
            PriceRule::SignificantFigures {
                max_figures: 5,
                step_floor: Price::parse("0.1").unwrap(),
            },
            Quantity::parse("0.001").unwrap(),
            Notional::parse("10").unwrap(),
        )
        .unwrap();
        let (low, high, spacing) = (
            Price::parse("90000").unwrap(),
            Price::parse("105000").unwrap(),
            Price::parse("2500").unwrap(),
        );

        let long = cell_prices(Direction::Long, low, high, spacing, &spec).unwrap();

        // 6 格，全部落在各自价位的步长上，价格就是理想位置。
        assert_eq!(long.len(), 6);
        for (step, price) in long.iter().enumerate() {
            let ideal = high.units() - step as i64 * spacing.units();
            assert_eq!(price.units(), ideal, "第 {step} 格不该被挪动");
            assert!(spec.price_rule().is_legal(*price));
        }
    }

    /// 两条腿都按 taker 收费：价差要盖过两份手续费才算盈利。
    #[test]
    fn a_spread_is_profitable_only_above_the_cost_of_both_legs() {
        let buy = Price::parse("100").unwrap();
        let taker = Rate::parse("0.00045").unwrap();

        // 100 → 100.1：价差 0.1，两份手续费约 0.09。
        assert!(spread_is_profitable(
            Direction::Long,
            buy,
            Price::parse("100.1").unwrap(),
            taker
        ));
        // 100 → 100.05：价差盖不住手续费。
        assert!(!spread_is_profitable(
            Direction::Long,
            buy,
            Price::parse("100.05").unwrap(),
            taker
        ));
        // 空头侧方向相反：开空 100 再买平 99.9 才盈利，买平 99.95 不盈利。
        assert!(spread_is_profitable(
            Direction::Short,
            buy,
            Price::parse("99.9").unwrap(),
            taker
        ));
        assert!(!spread_is_profitable(
            Direction::Short,
            buy,
            Price::parse("99.95").unwrap(),
            taker
        ));
    }

    #[test]
    fn the_minimum_spread_is_the_first_one_that_stays_profitable() {
        let buy = Price::parse("100").unwrap();
        let taker = Rate::parse("0.00045").unwrap();
        let tick = Price::parse("0.01").unwrap();

        let spread = minimum_grid_spread(Direction::Long, buy, taker, tick).unwrap();

        assert!(spread_is_profitable(
            Direction::Long,
            buy,
            buy.saturating_add(spread),
            taker
        ));
        // 少一个报价单位就不盈利：这是能满足约束的最小价差，不是保守的估计值。
        assert!(!spread_is_profitable(
            Direction::Long,
            buy,
            buy.saturating_add(spread).saturating_sub(tick),
            taker
        ));
    }

    /// 空头侧的平仓价在下方，最小价差按同样的费率算出来，但规整方向相反（向下）。
    #[test]
    fn b10_the_short_side_needs_the_same_spread_as_the_long_side() {
        let entry = Price::parse("100").unwrap();
        let taker = Rate::parse("0.00045").unwrap();
        let tick = Price::parse("0.01").unwrap();

        let short = minimum_grid_spread(Direction::Short, entry, taker, tick).unwrap();
        let long = minimum_grid_spread(Direction::Long, entry, taker, tick).unwrap();
        let exit = entry.saturating_sub(short);

        assert!(spread_is_profitable(Direction::Short, entry, exit, taker));
        // 平仓价抬高一个报价单位就不盈利：这是能满足约束的最小价差。
        assert!(!spread_is_profitable(
            Direction::Short,
            entry,
            exit.saturating_add(tick),
            taker
        ));
        // 两侧的盈亏平衡价几乎关于建仓价对称，规整到报价单位后最多差一个报价单位。
        assert!(
            short.units().abs_diff(long.units()) <= tick.units() as u64,
            "short {short} and long {long} differ by more than one tick"
        );
    }

    /// taker 费率不小于 100% 时任何价格上买入再卖出都亏：这是费率的问题，不是网格的问题。
    #[test]
    fn a_taker_rate_of_one_hundred_percent_can_never_be_profitable() {
        let error = minimum_grid_spread(
            Direction::Long,
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
