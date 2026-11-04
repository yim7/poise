//! 策略配置与配置校验。
//!
//! 校验都使用规整后的值：价格按报价精度规整，数量按数量精度向下规整。
//!
//! 一个策略最多有两侧（设计第 2.1 节）：只有多头侧是做多，只有空头侧是做空，
//! 两侧都有是中性网格。两侧用同一组字段，方向相反的部分由 [`Direction`] 表达，
//! 所以规整、划分和校验都是同一段代码。

use chrono::Duration;
use serde::{Deserialize, Serialize};

use crate::batch::{BatchPlan, build_batches};
use crate::error::ConfigError;
use crate::grid::{
    Cell, GridPlan, GridShape, build_cells, cell_count, cell_prices, max_cell_count,
    meets_minimum_cell_notional, minimum_grid_spread, required_cell_quantity, spread_is_profitable,
};
use crate::instrument::{Fees, Instrument, InstrumentSpec};
use crate::side::{Direction, SidePlan};
use crate::units::{Price, PriceRounding, Quantity, Rate, SCALE};

/// 网格格数的技术上限。
///
/// 设计没有规定这个上限。它只用来挡住明显不合理的配置（例如范围极大、间距极小），
/// 正常配置远达不到；按 Hyperliquid 每日 5 万次操作上限，格数超过这个量级本身也不可用。
pub const MAX_CELL_COUNT: usize = 10_000;

/// 主仓倾斜的取值范围。设计没有规定；上限挡住把首批压到零的输入，也让权重和数量相乘
/// 不会越界（权重最大约 `(n − 1) × 100`，批数不超过 [`MAX_CELL_COUNT`]）。
pub const MIN_CORE_SKEW: Rate = Rate::from_units(SCALE / 100);
pub const MAX_CORE_SKEW: Rate = Rate::from_units(SCALE * 100);

/// 一侧的配置。
///
/// 多头侧在价格下方，空头侧在价格上方；空头侧是多头侧的镜像（第 5.1 节）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideConfig {
    /// 本侧建仓的价格范围下限 `L`。
    pub price_low: Price,
    /// 本侧建仓的价格范围上限 `H`。
    pub price_high: Price,
    /// 本侧最多持有的数量 `M`。
    pub max_position: Quantity,
    /// 主仓比例 `r`，取值 `[0, 1]`；0 表示没有主仓，格子自成单位，1 表示只积累主仓、没有网格。
    ///
    /// 中性网格两侧都必须是 0（第 5.2 节）。
    pub core_ratio: Rate,
    /// 主仓批数 `n`；`r = 0` 时不需要，写 `None`。
    pub batch_count: Option<u32>,
    /// 主仓倾斜：末批主仓 ÷ 首批主仓，中间线性过渡，大于 1 是低处（空头侧为高处）多买的
    /// 金字塔，小于 1 反过来。设了它，网格就按全侧均匀分，不再随批次（第 3.1 节）。
    /// `None` 表示不倾斜，保持 `M` 平均分给各批、网格随批的分法；`r = 0` 时不需要。
    #[serde(default)]
    pub core_skew: Option<Rate>,
    /// 网格间距 `d`。
    pub grid_spacing: Price,
    /// 网格价差 `g`：多头侧平仓价 = 建仓价 + g，空头侧平仓价 = 建仓价 − g。
    pub grid_spread: Price,
    /// 限速间隔 τ（可选，第 4.1 节）：本侧在 τ 时间内最多启动一个单位；`None` 表示不限速。
    ///
    /// 它是运行中可以修改的参数（第 2.4 节），所以在配置里而不是状态里。
    pub throttle_interval: Option<Duration>,
    /// 观点失效（可选，第 7.5 节）：`None` 表示这一侧不启用自动止损，由用户自己管理。
    ///
    /// 和限速一样是运行中可以修改的参数（第 2.4 节）。
    #[serde(default)]
    pub invalidation: Option<Invalidation>,
}

/// 观点失效（第 7.5 节）：标记价越过失效价，说明这一侧的观点不成立。撤销本策略全部挂单，
/// 用只减仓单把本侧仓位减到剩余数量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invalidation {
    /// 失效价：多头侧必须低于 `L`，空头侧必须高于 `H`。
    pub price: Price,
    /// 失效后剩余数量 `R`：0 表示全部平仓；不超过本侧主仓总量；中性网格必须是 0。
    pub remaining: Quantity,
    /// 失效平仓单相对标记价最多让出多少，例如 0.01 是 1%。
    pub max_slippage: Rate,
}

/// 策略配置：标的与最多两侧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub instrument: Instrument,
    /// 多头侧；`None` 表示不做多。
    pub long: Option<SideConfig>,
    /// 空头侧；`None` 表示不做空。
    pub short: Option<SideConfig>,
}

/// 规整后的配置、两侧的批次与格子划分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NormalizedConfig {
    pub config: StrategyConfig,
    pub sides: Vec<SidePlan>,
}

impl StrategyConfig {
    /// 这一侧的配置；没有配置这一侧时是 `None`。
    pub fn side(&self, direction: Direction) -> Option<&SideConfig> {
        match direction {
            Direction::Long => self.long.as_ref(),
            Direction::Short => self.short.as_ref(),
        }
    }

    /// 规整两侧配置、划分批次与格子并校验。
    ///
    /// 两侧按 [`Direction::ALL`] 的顺序规整：同时有多处不合法时报出来的是多头侧那一个。
    pub(crate) fn normalize(
        &self,
        spec: &InstrumentSpec,
        fees: &Fees,
    ) -> Result<NormalizedConfig, ConfigError> {
        self.check_instrument(spec)?;
        // 符号检查放在规整之前：规整只对非负值有意义，负价格、负间距、负数量
        // 应该在这里变成配置错误，而不是走到规整里的断言上。
        self.check_signs()?;

        let mut sides = Vec::with_capacity(Direction::ALL.len());
        for direction in Direction::ALL {
            if let Some(side) = self.side(direction) {
                sides.push(normalize_side(direction, side, spec, fees)?);
            }
        }
        if sides.is_empty() {
            return Err(ConfigError::NoSidesConfigured);
        }
        check_sides_apart(&sides)?;

        let config = StrategyConfig {
            instrument: self.instrument.clone(),
            long: normalized_side_config(&sides, Direction::Long),
            short: normalized_side_config(&sides, Direction::Short),
        };
        Ok(NormalizedConfig { config, sides })
    }

    /// 配置的标的必须和交易所的一致。
    fn check_instrument(&self, spec: &InstrumentSpec) -> Result<(), ConfigError> {
        if &self.instrument == spec.instrument() {
            return Ok(());
        }
        Err(ConfigError::InstrumentMismatch {
            config: self.instrument.symbol().to_string(),
            spec: spec.instrument().symbol().to_string(),
        })
    }

    /// 规整之前先看符号：价格、数量、比例都不接受负值。
    ///
    /// 规整函数只对非负值有意义，先检查才能把普通非法输入变成可处理的
    /// [`ConfigError`]，而不是让调用方撞上规整里的断言。
    fn check_signs(&self) -> Result<(), ConfigError> {
        for direction in Direction::ALL {
            if let Some(side) = self.side(direction) {
                check_side_signs(direction, side)?;
            }
        }
        Ok(())
    }
}

fn check_side_signs(direction: Direction, side: &SideConfig) -> Result<(), ConfigError> {
    for (field, price) in [
        ("下限 L", side.price_low),
        ("上限 H", side.price_high),
        ("网格间距 d", side.grid_spacing),
        ("网格价差 g", side.grid_spread),
    ] {
        if price.units() < 0 {
            return Err(ConfigError::NegativePrice {
                direction,
                field,
                price,
            });
        }
    }
    if side.max_position.units() < 0 {
        return Err(ConfigError::NegativeMaxPosition {
            direction,
            max_position: side.max_position,
        });
    }
    if side.core_ratio.units() < 0 {
        return Err(ConfigError::CoreRatioOutOfRange {
            direction,
            core_ratio: side.core_ratio,
        });
    }
    // 限速间隔只用来算「下一笔放行还要等多久」，负值没有意义。
    if let Some(interval) = side.throttle_interval
        && interval < Duration::zero()
    {
        return Err(ConfigError::NegativeThrottleInterval {
            direction,
            interval,
        });
    }
    Ok(())
}

/// 规整一侧，然后划分它的批次与格子。
fn normalize_side(
    direction: Direction,
    side: &SideConfig,
    spec: &InstrumentSpec,
    fees: &Fees,
) -> Result<SidePlan, ConfigError> {
    let normalized = NormalizedSide::new(direction, side, spec)?;
    let prices = normalized.cell_prices(spec)?;
    let (batches, cells) = normalized.build_plan(&prices, spec)?;
    check_invalidation_remaining(direction, normalized.invalidation, &batches)?;
    check_cell_notional(&cells, &normalized, spec)?;
    check_cell_profitability(&cells, fees, &normalized)?;
    Ok(SidePlan::new(
        direction,
        normalized.to_config(),
        batches,
        GridPlan::from_cells(cells),
    ))
}

/// 两侧之间要留出空隙，而且中性网格两侧都没有主仓（设计第 2.1、5.2 节）。
///
/// 这里只保证两侧的建仓区间不重叠。平仓腿会伸到区间之外（多头侧的卖价到 `H + g`，
/// 空头侧的买价到 `L − g`），两侧的平仓腿仍可能落在同一个价格上，那由第 6 节的避让
/// 规则处理。
///
/// 主仓表达的是方向观点。HL 是单向持仓，两侧都留主仓时，一侧的开仓在交易所层面就是
/// 在平掉另一侧的主仓，所以中性网格不设主仓。
fn check_sides_apart(sides: &[SidePlan]) -> Result<(), ConfigError> {
    let (Some(long), Some(short)) = (
        side_plan(sides, Direction::Long),
        side_plan(sides, Direction::Short),
    ) else {
        return Ok(());
    };
    if long.config().price_high >= short.config().price_low {
        return Err(ConfigError::SidesOverlap {
            long_high: long.config().price_high,
            short_low: short.config().price_low,
        });
    }
    for side in sides {
        if !side.config().core_ratio.is_zero() {
            return Err(ConfigError::CoreInNeutralGrid {
                direction: side.direction(),
                core_ratio: side.config().core_ratio,
            });
        }
        // 中性网格任一侧失效都全部平仓（第 7.5 节），剩余数量只能是 0。
        if let Some(invalidation) = side.config().invalidation
            && !invalidation.remaining.is_zero()
        {
            return Err(ConfigError::InvalidationRemainingInNeutralGrid {
                direction: side.direction(),
                remaining: invalidation.remaining,
            });
        }
    }
    Ok(())
}

fn side_plan(sides: &[SidePlan], direction: Direction) -> Option<&SidePlan> {
    sides.iter().find(|side| side.direction() == direction)
}

fn normalized_side_config(sides: &[SidePlan], direction: Direction) -> Option<SideConfig> {
    side_plan(sides, direction).map(|side| side.config().clone())
}

/// 规整后的一侧参数：价格已按报价精度规整，数量已按数量精度向下规整。
///
/// 批次划分、格子划分和最终配置都由它构造，所以这些值只算一次。
struct NormalizedSide {
    direction: Direction,
    price_low: Price,
    price_high: Price,
    price_tick: Price,
    grid_spacing: Price,
    grid_spread: Price,
    max_position: Quantity,
    core_ratio: Rate,
    /// 主仓批数；主仓比例为 0 时没有批次，这里是 `None`。
    batch_count: Option<u32>,
    /// 主仓倾斜；主仓比例为 0 时没有批次，这里是 `None`。
    core_skew: Option<Rate>,
    /// 限速间隔不参与规整，原样带到最终配置里。
    throttle_interval: Option<Duration>,
    /// 失效设置：剩余数量已按数量精度向下规整；失效价是触发线，不按报价精度规整。
    invalidation: Option<Invalidation>,
}

impl NormalizedSide {
    /// 规整价格范围、数量、比例与批数。
    ///
    /// 检查顺序固定：价格范围 → 数量 → 比例与批数。这是原来那个大函数里的顺序，
    /// 同时有多处不合法时报出来的仍然是第一个。
    fn new(
        direction: Direction,
        side: &SideConfig,
        spec: &InstrumentSpec,
    ) -> Result<Self, ConfigError> {
        let price_low = normalize_price(direction, "下限 L", side.price_low, spec)?;
        let price_high = normalize_price(direction, "上限 H", side.price_high, spec)?;
        if price_low >= price_high {
            return Err(ConfigError::PriceRangeNotAscending {
                direction,
                low: price_low,
                high: price_high,
            });
        }
        // 报价规则随价格变粗，间距和价差是差值，按最高的那个价处的步长规整：
        // 网格里所有价格都在它下面，步长只会更细，这样规整过的间距处处都落得下。
        // 「至少一个报价单位」的校验留到生成格子时做：主仓比例为 1 时没有网格，
        // 间距与价差不参与计算，也就不该被拒绝。
        let price_tick = spec.price_rule().step_at(price_high);
        let grid_spacing =
            normalize_difference(direction, "网格间距 d", side.grid_spacing, price_tick)?;
        let grid_spread =
            normalize_difference(direction, "网格价差 g", side.grid_spread, price_tick)?;

        let max_position = spec.normalize_quantity(side.max_position);
        if max_position.units() < spec.quantity_step().units() {
            return Err(ConfigError::MaxPositionBelowOneStep {
                direction,
                max_position,
                quantity_step: spec.quantity_step(),
            });
        }

        let core_ratio = side.core_ratio;
        if core_ratio.units() > SCALE {
            return Err(ConfigError::CoreRatioOutOfRange {
                direction,
                core_ratio,
            });
        }
        // 主仓比例为 0 时没有批次，批数和倾斜不参与计算，写什么都不影响结果。
        let batch_count = match side.batch_count {
            _ if core_ratio.is_zero() => None,
            Some(count) if count >= 1 => Some(count),
            other => {
                return Err(ConfigError::BatchCountOutOfRange {
                    direction,
                    batch_count: other,
                });
            }
        };
        let core_skew = match side.core_skew {
            _ if core_ratio.is_zero() => None,
            Some(skew)
                if (MIN_CORE_SKEW.units()..=MAX_CORE_SKEW.units()).contains(&skew.units()) =>
            {
                Some(skew)
            }
            Some(skew) => {
                return Err(ConfigError::CoreSkewOutOfRange {
                    direction,
                    core_skew: skew,
                });
            }
            None => None,
        };

        Ok(Self {
            direction,
            price_low,
            price_high,
            price_tick,
            grid_spacing,
            grid_spread,
            max_position,
            core_ratio,
            batch_count,
            core_skew,
            throttle_interval: side.throttle_interval,
            invalidation: normalize_invalidation(direction, side, price_low, price_high, spec)?,
        })
    }

    /// 格子的建仓价，按启动顺序。
    ///
    /// 主仓比例为 1 时只积累主仓、没有网格（第 2.3 节）：不生成格子，网格间距、价差
    /// 和格子金额都不需要，也就不校验。否则先要求间距与价差至少一个报价精度，再按范围
    /// 与间距算出格数——先算格数再构造价格，极端配置不能因为分配内存而拖死进程。
    fn cell_prices(&self, spec: &InstrumentSpec) -> Result<Vec<Price>, ConfigError> {
        if self.core_ratio.units() == SCALE {
            return Ok(Vec::new());
        }
        if self.grid_spacing.units() < self.price_tick.units() {
            return Err(ConfigError::GridSpacingBelowOneTick {
                direction: self.direction,
                grid_spacing: self.grid_spacing,
                price_tick: self.price_tick,
            });
        }
        if self.grid_spread.units() < self.price_tick.units() {
            return Err(ConfigError::GridSpreadBelowOneTick {
                direction: self.direction,
                grid_spread: self.grid_spread,
                price_tick: self.price_tick,
            });
        }
        let count = cell_count(self.price_high, self.price_low, self.grid_spacing);
        if count > MAX_CELL_COUNT as i64 {
            return Err(ConfigError::TooManyCells {
                direction: self.direction,
                cell_count: count as usize,
                max_cell_count: MAX_CELL_COUNT,
            });
        }
        cell_prices(
            self.direction,
            self.price_low,
            self.price_high,
            self.grid_spacing,
            spec,
        )
    }

    /// 划分批次与格子。
    ///
    /// 先挡住过大的批数再划分批次：极端配置不能因为分配内存而拖死进程，具体的
    /// 「哪一批没有格子」由划分批次时报出。
    fn build_plan(
        &self,
        prices: &[Price],
        spec: &InstrumentSpec,
    ) -> Result<(BatchPlan, Vec<Cell>), ConfigError> {
        if let Some(batch_count) = self.batch_count
            && batch_count as usize > MAX_CELL_COUNT
        {
            return Err(ConfigError::TooManyBatches {
                direction: self.direction,
                batch_count,
                max_batch_count: MAX_CELL_COUNT,
            });
        }
        let shape = self.grid_shape();
        match self.batch_count {
            Some(batch_count) => build_batches(self.direction, prices, &shape, batch_count, spec),
            None => Ok((
                BatchPlan::default(),
                build_cells(self.direction, prices, &shape, spec)?,
            )),
        }
    }

    fn grid_shape(&self) -> GridShape {
        GridShape {
            price_low: self.price_low,
            price_high: self.price_high,
            max_position: self.max_position,
            core_ratio: self.core_ratio,
            core_skew: self.core_skew,
            grid_spacing: self.grid_spacing,
            grid_spread: self.grid_spread,
        }
    }

    /// 规整后的这一侧配置。
    fn to_config(&self) -> SideConfig {
        SideConfig {
            price_low: self.price_low,
            price_high: self.price_high,
            max_position: self.max_position,
            core_ratio: self.core_ratio,
            batch_count: self.batch_count,
            core_skew: self.core_skew,
            grid_spacing: self.grid_spacing,
            grid_spread: self.grid_spread,
            throttle_interval: self.throttle_interval,
            invalidation: self.invalidation,
        }
    }
}

/// 检查并规整失效设置（第 2.2、7.5 节，B41）：失效价在范围之外（多头侧低于 `L`，空头侧
/// 高于 `H`），滑点上限在 0 和 100% 之间。剩余数量按数量精度向下规整；它和主仓总量的比较
/// 要等批次划分出来（[`check_invalidation_remaining`]）。
fn normalize_invalidation(
    direction: Direction,
    side: &SideConfig,
    price_low: Price,
    price_high: Price,
    spec: &InstrumentSpec,
) -> Result<Option<Invalidation>, ConfigError> {
    let Some(invalidation) = side.invalidation else {
        return Ok(None);
    };
    let outside = match direction {
        Direction::Long => invalidation.price < price_low && invalidation.price.units() > 0,
        Direction::Short => invalidation.price > price_high,
    };
    if !outside {
        return Err(ConfigError::InvalidationPriceInsideRange {
            direction,
            price: invalidation.price,
            edge: match direction {
                Direction::Long => price_low,
                Direction::Short => price_high,
            },
        });
    }
    if invalidation.max_slippage.units() <= 0 || invalidation.max_slippage.units() >= SCALE {
        return Err(ConfigError::InvalidationSlippageOutOfRange {
            direction,
            max_slippage: invalidation.max_slippage,
        });
    }
    if invalidation.remaining.units() < 0 {
        return Err(ConfigError::NegativeMaxPosition {
            direction,
            max_position: invalidation.remaining,
        });
    }
    Ok(Some(Invalidation {
        remaining: spec.normalize_quantity(invalidation.remaining),
        ..invalidation
    }))
}

/// 失效后剩余数量不超过本侧主仓总量（第 2.2 节，B41）：失效之后留下的应当是主仓，网格
/// 仓位先减掉。
fn check_invalidation_remaining(
    direction: Direction,
    invalidation: Option<Invalidation>,
    batches: &BatchPlan,
) -> Result<(), ConfigError> {
    let Some(invalidation) = invalidation else {
        return Ok(());
    };
    let core_total = batches
        .batches()
        .iter()
        .fold(Quantity::ZERO, |total, batch| total + batch.core_quantity);
    if invalidation.remaining > core_total {
        return Err(ConfigError::InvalidationRemainingAboveCore {
            direction,
            remaining: invalidation.remaining,
            core_total,
        });
    }
    Ok(())
}

/// 每一格的金额都要够最低下单金额。
///
/// 按两条腿中较低的价格计算（第 2.3 节）：多头侧的最低腿是格子建仓价，空头侧是
/// 买入平仓价。这里逐格检查，不依赖这条推理；从两条腿里较低价最低的一格开始报，
/// 它最严格。没有网格时 `cells` 是空的，检查自然跳过。
fn check_cell_notional(
    cells: &[Cell],
    side: &NormalizedSide,
    spec: &InstrumentSpec,
) -> Result<(), ConfigError> {
    let mut ordered: Vec<&Cell> = cells.iter().collect();
    ordered.sort_by_key(|cell| cell.lower_price());
    for cell in ordered {
        let price = cell.lower_price();
        if meets_minimum_cell_notional(cell.quantity, price, spec) {
            continue;
        }
        return Err(ConfigError::CellBelowMinimumOrderNotional {
            direction: side.direction,
            cell_index: cell.index,
            cell_price: price,
            cell_quantity: cell.quantity,
            max_cell_count: max_cell_count(side.direction, cells.len(), &side.grid_shape(), spec),
            required_quantity: required_cell_quantity(price, spec),
        });
    }
    Ok(())
}

/// 每一格的价差要覆盖两条腿的 taker 费率（第 2.3 节）。建仓价最高的一格最严格。
fn check_cell_profitability(
    cells: &[Cell],
    fees: &Fees,
    side: &NormalizedSide,
) -> Result<(), ConfigError> {
    let mut ordered: Vec<&Cell> = cells.iter().collect();
    ordered.sort_by_key(|cell| std::cmp::Reverse(cell.entry_price));
    for cell in ordered {
        if spread_is_profitable(
            side.direction,
            cell.entry_price,
            cell.exit_price,
            fees.taker_rate,
        ) {
            continue;
        }
        let minimum_grid_spread = minimum_grid_spread(
            side.direction,
            cell.entry_price,
            fees.taker_rate,
            side.price_tick,
        )?;
        return Err(ConfigError::GridSpreadNotProfitable {
            direction: side.direction,
            grid_spread: side.grid_spread,
            minimum_grid_spread,
            taker_fee_rate: fees.taker_rate,
        });
    }
    Ok(())
}

/// 按这个价格处的报价精度规整；规整结果超出可表示范围时是配置错误，不做饱和。
fn normalize_price(
    direction: Direction,
    field: &'static str,
    price: Price,
    spec: &InstrumentSpec,
) -> Result<Price, ConfigError> {
    spec.normalize_price(price, PriceRounding::Nearest)
        .ok_or(ConfigError::PriceOutOfRange {
            direction,
            field,
            price,
            price_tick: spec.price_rule().step_at(price),
        })
}

/// 把差值（网格间距、价差）规整到 `price_tick` 的整数倍。
///
/// 差值是「两个价格之间的距离」，不是价格本身：它的精度要按它所在区间的最高价算，
/// 也就是网格里最粗的那个步长，否则低位处规整出来的间距在高位处落不下。
fn normalize_difference(
    direction: Direction,
    field: &'static str,
    value: Price,
    price_tick: Price,
) -> Result<Price, ConfigError> {
    value
        .checked_round_to_tick(price_tick, PriceRounding::Nearest)
        .ok_or(ConfigError::PriceOutOfRange {
            direction,
            field,
            price: value,
            price_tick,
        })
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, MAX_CELL_COUNT, NormalizedConfig, SideConfig, StrategyConfig};
    use crate::batch::BatchIndex;
    use crate::fixtures::{
        btc_fees, btc_spec, core_long_config, core_only_config, fine_spec, hl_btc_spec,
        is_quotable, long_grid_config, significant_figures, skewed_core_long_config,
    };
    use crate::grid::{CellIndex, required_cell_quantity};
    use crate::instrument::{Fees, Instrument, InstrumentSpec, PriceRule};
    use crate::side::{Direction, SidePlan};
    use crate::units::{Notional, Price, Quantity, Rate};
    use chrono::Duration;

    /// 测试里大多只配多头侧：`Option` 的包装不值得在每个用例里重复。
    fn long_side(config: &StrategyConfig) -> &SideConfig {
        config.long.as_ref().expect("the long side is configured")
    }

    fn long_side_mut(config: &mut StrategyConfig) -> &mut SideConfig {
        config.long.as_mut().expect("the long side is configured")
    }

    /// 只有多头侧的配置，其余字段沿用 [`long_grid_config`]。
    fn with_long(side: SideConfig) -> StrategyConfig {
        StrategyConfig {
            long: Some(side),
            ..long_grid_config()
        }
    }

    /// 规整结果里多头侧的划分。
    fn long_plan(normalized: &NormalizedConfig) -> &SidePlan {
        normalized
            .sides
            .iter()
            .find(|side| side.direction() == Direction::Long)
            .expect("the long side is configured")
    }

    /// 没有主仓的配置，价格范围与数量按参数给定。
    fn config(price_low: &str, price_high: &str, max_position: &str) -> StrategyConfig {
        with_long(SideConfig {
            price_low: Price::parse(price_low).unwrap(),
            price_high: Price::parse(price_high).unwrap(),
            max_position: Quantity::parse(max_position).unwrap(),
            core_ratio: Rate::ZERO,
            batch_count: None,
            core_skew: None,
            grid_spacing: Price::parse("1000").unwrap(),
            grid_spread: Price::parse("1000").unwrap(),
            throttle_interval: None,
            invalidation: None,
        })
    }

    /// 设计第 3.2 节的例子加上主仓：`r = .7`，`n = 4`。
    ///
    /// 每批 `q = .25`，主仓 `C = .175`，网格 `G = .075`；每批 5 格，每格 `.015`。
    fn core_config() -> StrategyConfig {
        core_config_with(4)
    }

    fn core_config_with(batch_count: u32) -> StrategyConfig {
        StrategyConfig {
            long: Some(SideConfig {
                core_ratio: Rate::parse("0.7").unwrap(),
                batch_count: Some(batch_count),
                core_skew: None,
                ..config("75000", "95000", "1").long.unwrap()
            }),
            ..config("75000", "95000", "1")
        }
    }

    /// 间距和价差可调的那份配置，用来构造跨价位的网格。
    fn hl_config(
        price_low: &str,
        price_high: &str,
        grid_spacing: &str,
        grid_spread: &str,
        max_position: &str,
    ) -> StrategyConfig {
        with_long(SideConfig {
            price_low: Price::parse(price_low).unwrap(),
            price_high: Price::parse(price_high).unwrap(),
            max_position: Quantity::parse(max_position).unwrap(),
            core_ratio: Rate::ZERO,
            batch_count: None,
            core_skew: None,
            grid_spacing: Price::parse(grid_spacing).unwrap(),
            grid_spread: Price::parse(grid_spread).unwrap(),
            throttle_interval: None,
            invalidation: None,
        })
    }

    #[test]
    fn normalizes_prices_to_tick_and_quantity_down_to_step() {
        let mut raw = long_grid_config();
        long_side_mut(&mut raw).price_low = Price::parse("75000.4").unwrap();
        long_side_mut(&mut raw).price_high = Price::parse("95000.4").unwrap();
        long_side_mut(&mut raw).max_position = Quantity::parse("1.00049").unwrap();

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert_eq!(
            long_side(&normalized.config).price_low,
            Price::parse("75000").unwrap()
        );
        assert_eq!(
            long_side(&normalized.config).price_high,
            Price::parse("95000").unwrap()
        );
        assert_eq!(
            long_side(&normalized.config).max_position,
            Quantity::parse("1").unwrap()
        );
        assert_eq!(
            long_plan(&normalized).grid().cells()[0].entry_price,
            Price::parse("95000").unwrap()
        );
        assert_eq!(
            long_plan(&normalized).grid().cells()[19].entry_price,
            Price::parse("76000").unwrap()
        );
    }

    #[test]
    fn grid_cells_start_at_high_and_carry_the_spread() {
        let normalized = long_grid_config()
            .normalize(&btc_spec(), &btc_fees())
            .unwrap();

        let cells = long_plan(&normalized).grid().cells();
        assert_eq!(cells.len(), 20);
        assert_eq!(cells[0].entry_price, Price::parse("95000").unwrap());
        assert_eq!(cells[0].exit_price, Price::parse("96000").unwrap());
        assert_eq!(cells[19].entry_price, Price::parse("76000").unwrap());
        assert_eq!(cells[19].exit_price, Price::parse("77000").unwrap());
        assert!(
            cells
                .iter()
                .all(|cell| cell.quantity == Quantity::parse("0.05").unwrap())
        );
        assert!(cells.iter().all(|cell| cell.batch.is_none()));
        assert!(long_plan(&normalized).batches().is_empty());
    }

    #[test]
    fn cell_quantities_give_the_remainder_to_the_cells_nearest_the_gap() {
        let raw = config("75000", "95000", "0.063");

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        let cells = long_plan(&normalized).grid().cells();
        assert_eq!(cells.len(), 20);
        for cell in &cells[..3] {
            assert_eq!(cell.quantity, Quantity::parse("0.004").unwrap());
        }
        for cell in &cells[3..] {
            assert_eq!(cell.quantity, Quantity::parse("0.003").unwrap());
        }
        let total = cells
            .iter()
            .fold(Quantity::ZERO, |total, cell| total + cell.quantity);
        assert_eq!(total, Quantity::parse("0.063").unwrap());
    }

    #[test]
    fn core_batches_split_the_range_and_keep_the_core_ratio() {
        let normalized = core_config().normalize(&btc_spec(), &btc_fees()).unwrap();

        let batches = long_plan(&normalized).batches().batches();
        assert_eq!(batches.len(), 4);
        for (index, batch) in batches.iter().enumerate() {
            assert_eq!(batch.index, BatchIndex(index));
            assert_eq!(
                batch.price,
                Price::parse(["95000", "90000", "85000", "80000"][index]).unwrap()
            );
            assert_eq!(batch.quantity, Quantity::parse("0.25").unwrap());
            assert_eq!(batch.core_quantity, Quantity::parse("0.175").unwrap());
            assert_eq!(batch.grid_quantity, Quantity::parse("0.075").unwrap());
            assert_eq!(batch.cells.len(), 5);
        }
        // 每批的网格部分平分给跟随它的格子：95 到 91 跟随 95 批，依此类推。
        for (index, cell) in long_plan(&normalized).grid().cells().iter().enumerate() {
            assert_eq!(cell.batch, Some(BatchIndex(index / 5)));
            assert_eq!(cell.quantity, Quantity::parse("0.015").unwrap());
        }
    }

    #[test]
    fn b27_batch_quantities_give_the_remainder_to_the_batches_nearest_the_gap() {
        let raw = core_config_with(3);

        let normalized = raw.normalize(&fine_spec(), &btc_fees()).unwrap();

        let batches = long_plan(&normalized).batches().batches();
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].quantity, Quantity::parse("0.33334").unwrap());
        assert_eq!(batches[1].quantity, Quantity::parse("0.33333").unwrap());
        assert_eq!(batches[2].quantity, Quantity::parse("0.33333").unwrap());
        let total = batches
            .iter()
            .fold(Quantity::ZERO, |total, batch| total + batch.quantity);
        assert_eq!(total, Quantity::parse("1").unwrap());
        // 每批的主仓按比例向上取整到最小单位，网格部分是剩下的。
        for batch in batches {
            assert_eq!(batch.core_quantity, Quantity::parse("0.23334").unwrap());
        }
    }

    #[test]
    fn b27_cell_quantities_give_the_remainder_to_the_cells_nearest_the_batch_price() {
        let raw = core_config_with(3);

        let normalized = raw.normalize(&fine_spec(), &btc_fees()).unwrap();

        // 95 批的网格部分是 0.1，分给 7 格：余数给靠近批次价的 4 格。
        let batch = &long_plan(&normalized).batches().batches()[0];
        assert_eq!(batch.cells.len(), 7);
        assert_eq!(batch.grid_quantity, Quantity::parse("0.1").unwrap());
        let cells = long_plan(&normalized).grid().cells();
        for cell in &cells[..4] {
            assert_eq!(cell.quantity, Quantity::parse("0.01429").unwrap());
        }
        for cell in &cells[4..7] {
            assert_eq!(cell.quantity, Quantity::parse("0.01428").unwrap());
        }
        let total = cells[..7]
            .iter()
            .fold(Quantity::ZERO, |total, cell| total + cell.quantity);
        assert_eq!(total, Quantity::parse("0.1").unwrap());
    }

    #[test]
    fn b28_rejects_a_batch_whose_segment_has_no_cell() {
        let mut raw = core_config();
        long_side_mut(&mut raw).grid_spacing = Price::parse("8000").unwrap();

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        // 8000 的间距下只有 95000 和 87000 两格：85 批的段里没有格子。
        assert_eq!(
            error,
            ConfigError::BatchWithoutCell {
                direction: Direction::Long,
                batch_index: BatchIndex(2),
                batch_price: Price::parse("85000").unwrap(),
                segment_low: Price::parse("80000").unwrap(),
                segment_high: Price::parse("85000").unwrap(),
            }
        );
        assert_eq!(
            error.to_string(),
            "做多侧：第 2 批（85000）的区间 (80000, 85000] 里没有格子；减小网格间距 d 或减少批数 n"
        );

        // 缩小网格间距，每批的段内就都有格子了。
        let mut denser = raw.clone();
        long_side_mut(&mut denser).grid_spacing = Price::parse("1000").unwrap();
        assert!(denser.normalize(&btc_spec(), &btc_fees()).is_ok());
    }

    /// 空头侧的批次价从 `L` 往上，报错里的价格段也要按这个方向写：段的下限是这一批
    /// 自己的价格，上限是相邻的那一批。
    #[test]
    fn b28_the_short_batch_segment_follows_the_short_direction() {
        let short = |spacing: &str| StrategyConfig {
            long: None,
            short: Some(SideConfig {
                core_ratio: Rate::parse("0.7").unwrap(),
                batch_count: Some(4),
                core_skew: None,
                grid_spacing: Price::parse(spacing).unwrap(),
                ..config("75000", "95000", "1").long.unwrap()
            }),
            ..config("75000", "95000", "1")
        };

        // 8000 的间距下只有 75000、83000、91000 三格：批次价 85000 那一批负责
        // 85000 到 90000 这一段，段里没有格子。
        assert_eq!(
            short("8000")
                .normalize(&btc_spec(), &btc_fees())
                .unwrap_err(),
            ConfigError::BatchWithoutCell {
                direction: Direction::Short,
                batch_index: BatchIndex(2),
                batch_price: Price::parse("85000").unwrap(),
                segment_low: Price::parse("85000").unwrap(),
                segment_high: Price::parse("90000").unwrap(),
            }
        );

        // 7000 的间距下最高的一格是 89000：最后一批负责 90000 到 `H` 这一段，段的
        // 另一端就是这一侧远离空隙的那一端。
        assert_eq!(
            short("7000")
                .normalize(&btc_spec(), &btc_fees())
                .unwrap_err(),
            ConfigError::BatchWithoutCell {
                direction: Direction::Short,
                batch_index: BatchIndex(3),
                batch_price: Price::parse("90000").unwrap(),
                segment_low: Price::parse("90000").unwrap(),
                segment_high: Price::parse("95000").unwrap(),
            }
        );
    }

    #[test]
    fn rejects_a_core_ratio_outside_zero_to_one() {
        let mut negative = core_config();
        long_side_mut(&mut negative).core_ratio = Rate::parse("-0.1").unwrap();
        assert_eq!(
            negative.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::CoreRatioOutOfRange {
                direction: Direction::Long,
                core_ratio: Rate::parse("-0.1").unwrap(),
            }
        );

        // 1 是允许的（第 2.3 节），再往上就不行。
        let mut above = core_config();
        long_side_mut(&mut above).core_ratio = Rate::parse("1.00000001").unwrap();
        assert_eq!(
            above.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::CoreRatioOutOfRange {
                direction: Direction::Long,
                core_ratio: Rate::parse("1.00000001").unwrap(),
            }
        );
    }

    #[test]
    fn b28_a_full_core_ratio_builds_batches_without_a_grid() {
        let normalized = core_only_config()
            .normalize(&btc_spec(), &btc_fees())
            .unwrap();

        // 没有格子：网格间距、价差和格子金额都不校验，只校验每批的主仓买入金额。
        assert!(long_plan(&normalized).grid().is_empty());
        let batches = long_plan(&normalized).batches().batches();
        assert_eq!(batches.len(), 4);
        for (index, batch) in batches.iter().enumerate() {
            assert_eq!(
                batch.price,
                Price::parse(["95000", "90000", "85000", "80000"][index]).unwrap()
            );
            assert_eq!(batch.quantity, Quantity::parse("0.25").unwrap());
            assert_eq!(batch.core_quantity, Quantity::parse("0.25").unwrap());
            assert_eq!(batch.grid_quantity, Quantity::ZERO);
            assert!(batch.cells.is_empty());
        }

        // 间距和价差在 `r = 1` 下不参与校验：同样的值在没有主仓时会被拒绝。
        let mut core_only = core_only_config();
        long_side_mut(&mut core_only).grid_spacing = Price::parse("0.4").unwrap();
        long_side_mut(&mut core_only).grid_spread = Price::parse("0.4").unwrap();
        assert!(core_only.normalize(&btc_spec(), &btc_fees()).is_ok());

        let mut grid = long_grid_config();
        long_side_mut(&mut grid).grid_spacing = Price::parse("0.4").unwrap();
        assert!(matches!(
            grid.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::GridSpacingBelowOneTick { .. }
        ));
    }

    #[test]
    fn a_full_core_ratio_still_checks_the_per_batch_core_buy() {
        // 数量精度更细：每批只剩 0.0001，在 95000 的批次价下不到最低金额 10 美元。
        let mut raw = core_only_config();
        long_side_mut(&mut raw).max_position = Quantity::parse("0.0004").unwrap();

        assert_eq!(
            raw.normalize(&fine_spec(), &btc_fees()).unwrap_err(),
            ConfigError::BatchCoreBelowMinimumOrderNotional {
                direction: Direction::Long,
                batch_index: BatchIndex(0),
                batch_price: Price::parse("95000").unwrap(),
                core_quantity: Quantity::parse("0.0001").unwrap(),
                required_quantity: Quantity::parse("0.00011").unwrap(),
            }
        );
    }

    #[test]
    fn rejects_negative_prices_quantities_and_rates_before_normalizing() {
        let spec = btc_spec();
        let fees = btc_fees();

        // 规整只对非负值有意义：这些输入要在规整之前变成配置错误。
        let mut negative_low = long_grid_config();
        long_side_mut(&mut negative_low).price_low = Price::parse("-1").unwrap();
        assert!(matches!(
            negative_low.normalize(&spec, &fees).unwrap_err(),
            ConfigError::NegativePrice { .. }
        ));

        let mut negative_spacing = long_grid_config();
        long_side_mut(&mut negative_spacing).grid_spacing = Price::parse("-1000").unwrap();
        assert!(matches!(
            negative_spacing.normalize(&spec, &fees).unwrap_err(),
            ConfigError::NegativePrice { .. }
        ));

        let mut negative_spread = long_grid_config();
        long_side_mut(&mut negative_spread).grid_spread = Price::parse("-1000").unwrap();
        assert!(matches!(
            negative_spread.normalize(&spec, &fees).unwrap_err(),
            ConfigError::NegativePrice { .. }
        ));

        let mut negative_quantity = long_grid_config();
        long_side_mut(&mut negative_quantity).max_position = Quantity::parse("-1").unwrap();
        assert!(matches!(
            negative_quantity.normalize(&spec, &fees).unwrap_err(),
            ConfigError::NegativeMaxPosition { .. }
        ));

        let mut negative_ratio = core_config();
        long_side_mut(&mut negative_ratio).core_ratio = Rate::parse("-0.5").unwrap();
        assert!(matches!(
            negative_ratio.normalize(&spec, &fees).unwrap_err(),
            ConfigError::CoreRatioOutOfRange {
                direction: Direction::Long,
                ..
            }
        ));
    }

    /// 价格本身能表示，加上价差就越过上限：饱和出来的价格不是报价精度的整数倍，
    /// 会变成一张无法合法报出的订单。这里必须直接报配置错误。
    #[test]
    fn b23_rejects_a_sell_price_that_does_not_fit_instead_of_saturating() {
        for (low, high, spread) in [
            ("90000000000", "90000000001", "10000000000"),
            ("92233720367", "92233720368", "1"),
        ] {
            let raw = with_long(SideConfig {
                price_low: Price::parse(low).unwrap(),
                price_high: Price::parse(high).unwrap(),
                max_position: Quantity::parse("1").unwrap(),
                grid_spacing: Price::parse("1").unwrap(),
                grid_spread: Price::parse(spread).unwrap(),
                ..long_grid_config().long.unwrap()
            });

            assert_eq!(
                raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
                ConfigError::ExitPriceOutOfRange {
                    direction: Direction::Long,
                    cell_index: CellIndex(0),
                    entry_price: Price::parse(high).unwrap(),
                    grid_spread: Price::parse(spread).unwrap(),
                },
                "low = {low}, high = {high}, spread = {spread}"
            );
        }
    }

    /// 按报价精度规整可能超出可表示范围：`H = 92233720368.54` 按一个报价单位
    /// 四舍五入得到 `92233720369`，它的最小单位超过 `i64::MAX`。
    /// 这必须是配置错误，不能是 debug 构建里的 panic。
    #[test]
    fn b23_rejects_a_price_that_rounds_out_of_the_representable_range() {
        let raw = with_long(SideConfig {
            price_low: Price::parse("92233720367").unwrap(),
            price_high: Price::parse("92233720368.54").unwrap(),
            max_position: Quantity::parse("1").unwrap(),
            core_ratio: Rate::ZERO,
            batch_count: None,
            core_skew: None,
            grid_spacing: Price::parse("1").unwrap(),
            grid_spread: Price::parse("1").unwrap(),
            throttle_interval: None,
            invalidation: None,
        });

        assert_eq!(
            raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::PriceOutOfRange {
                direction: Direction::Long,
                field: "上限 H",
                price: Price::parse("92233720368.54").unwrap(),
                price_tick: Price::parse("1").unwrap(),
            }
        );
    }

    /// 报价精度 `2e-8`、间距 1：第 1 批的精确批次价是 `174.999999988`，按报价精度
    /// 规整一次得到 `174.99999998`。先把分段偏移取整到 `1e-8`、再规整一次会得到
    /// `175`，于是买价 175 的格子从第 0 批落到第 1 批。
    #[test]
    fn b23_a_batch_price_is_rounded_to_the_tick_only_once() {
        let spec = InstrumentSpec::new(
            Instrument::try_new("BTC").unwrap(),
            Price::parse("0.00000002").unwrap(),
            Quantity::parse("0.001").unwrap(),
            Notional::parse("10").unwrap(),
        )
        .unwrap();
        let raw = with_long(SideConfig {
            price_low: Price::parse("74.99999994").unwrap(),
            price_high: Price::parse("200").unwrap(),
            max_position: Quantity::parse("1000").unwrap(),
            core_ratio: Rate::parse("0.7").unwrap(),
            batch_count: Some(5),
            core_skew: None,
            grid_spacing: Price::parse("1").unwrap(),
            grid_spread: Price::parse("1").unwrap(),
            throttle_interval: None,
            invalidation: None,
        });

        let normalized = raw.normalize(&spec, &btc_fees()).unwrap();

        assert_eq!(
            long_plan(&normalized)
                .batches()
                .batch(BatchIndex(1))
                .unwrap()
                .price,
            Price::parse("174.99999998").unwrap()
        );
        let cell = long_plan(&normalized)
            .grid()
            .cells()
            .iter()
            .find(|cell| cell.entry_price == Price::parse("175").unwrap())
            .expect("the grid has a cell at 175");
        assert_eq!(
            cell.batch,
            Some(BatchIndex(0)),
            "175 is above the exact batch 1 price, so it follows batch 0"
        );
    }

    /// 费率本身就不可能盈利时，错误要指向费率，而不是让用户去调网格。
    #[test]
    fn b23_reports_a_taker_fee_rate_that_can_never_be_profitable() {
        // 费率写错一个量级：1.5 而不是 0.0015。
        let fees = Fees::new(Rate::parse("1.5").unwrap(), Rate::parse("0.00015").unwrap());

        assert_eq!(
            long_grid_config()
                .normalize(&btc_spec(), &fees)
                .unwrap_err(),
            ConfigError::TakerFeeRateTooHigh {
                taker_fee_rate: Rate::parse("1.5").unwrap(),
            }
        );
    }

    #[test]
    fn b23_reports_an_unreachable_grid_spread_instead_of_searching_for_one() {
        // 买价接近可表示上限：卖价只加得上一个报价单位，而扣费后盈利要求的价差
        // 远超这个范围。这里必须直接给出配置错误，不能逐个报价单位试探。
        let raw = with_long(SideConfig {
            price_low: Price::parse("92233720300").unwrap(),
            price_high: Price::parse("92233720301").unwrap(),
            max_position: Quantity::parse("1").unwrap(),
            grid_spacing: Price::parse("1").unwrap(),
            grid_spread: Price::parse("1").unwrap(),
            ..long_grid_config().long.unwrap()
        });

        assert_eq!(
            raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::GridSpreadUnreachable {
                direction: Direction::Long,
                entry_price: Price::parse("92233720301").unwrap(),
                taker_fee_rate: Rate::parse("0.00045").unwrap(),
            }
        );
    }

    #[test]
    fn rejects_a_missing_or_zero_batch_count_when_the_core_ratio_is_above_zero() {
        let mut missing = core_config();
        long_side_mut(&mut missing).batch_count = None;
        assert_eq!(
            missing.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::BatchCountOutOfRange {
                direction: Direction::Long,
                batch_count: None
            }
        );

        let mut zero = core_config();
        long_side_mut(&mut zero).batch_count = Some(0);
        assert_eq!(
            zero.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::BatchCountOutOfRange {
                direction: Direction::Long,
                batch_count: Some(0)
            }
        );
    }

    #[test]
    fn rejects_more_batches_than_the_supported_maximum_before_allocating() {
        let mut raw = core_config();
        long_side_mut(&mut raw).batch_count = Some(u32::MAX);

        assert_eq!(
            raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::TooManyBatches {
                direction: Direction::Long,
                batch_count: u32::MAX,
                max_batch_count: MAX_CELL_COUNT,
            }
        );
    }

    #[test]
    fn ignores_the_batch_count_when_the_core_ratio_is_zero() {
        let mut raw = long_grid_config();
        long_side_mut(&mut raw).batch_count = Some(3);

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert!(long_plan(&normalized).batches().is_empty());
        assert_eq!(long_side(&normalized.config).batch_count, None);
        assert_eq!(long_plan(&normalized).grid().cells().len(), 20);
    }

    /// 主仓倾斜 3：主仓总量 .7 按 3 : 5 : 7 : 9 分到四批，网格 .3 平均分给 20 格。
    #[test]
    fn a_core_skew_tilts_the_core_and_spreads_the_grid_evenly() {
        let normalized = skewed_core_long_config()
            .normalize(&btc_spec(), &btc_fees())
            .unwrap();

        let plan = long_plan(&normalized);
        let batches = plan.batches().batches();
        let core: Vec<Quantity> = batches.iter().map(|batch| batch.core_quantity).collect();
        assert_eq!(
            core,
            ["0.088", "0.146", "0.204", "0.262"]
                .map(|value| Quantity::parse(value).unwrap())
                .to_vec()
        );
        // 每批 5 格，每格 .015，和没有倾斜时一样；每批数量 = 主仓 + 5 × .015。
        assert!(
            plan.grid()
                .cells()
                .iter()
                .all(|cell| cell.quantity == Quantity::parse("0.015").unwrap())
        );
        let quantities: Vec<Quantity> = batches.iter().map(|batch| batch.quantity).collect();
        assert_eq!(
            quantities,
            ["0.163", "0.221", "0.279", "0.337"]
                .map(|value| Quantity::parse(value).unwrap())
                .to_vec()
        );
        let total = batches
            .iter()
            .fold(Quantity::ZERO, |total, batch| total + batch.quantity);
        assert_eq!(total, Quantity::parse("1").unwrap());
        assert_eq!(
            long_side(&normalized.config).core_skew,
            Some(Rate::parse("3").unwrap())
        );
    }

    /// 倾斜小于 1 是倒金字塔：首批最大。
    #[test]
    fn a_core_skew_below_one_puts_more_in_the_first_batch() {
        let mut raw = skewed_core_long_config();
        long_side_mut(&mut raw).core_skew = Some(Rate::parse("0.5").unwrap());

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        let core: Vec<Quantity> = long_plan(&normalized)
            .batches()
            .batches()
            .iter()
            .map(|batch| batch.core_quantity)
            .collect();
        assert!(core.windows(2).all(|pair| pair[0] > pair[1]), "{core:?}");
        assert_eq!(
            core.iter()
                .fold(Quantity::ZERO, |total, core| total + *core),
            Quantity::parse("0.7").unwrap()
        );
    }

    #[test]
    fn rejects_a_core_skew_outside_the_allowed_range() {
        for value in ["0", "0.009", "100.5", "-1"] {
            let mut raw = skewed_core_long_config();
            long_side_mut(&mut raw).core_skew = Some(Rate::parse(value).unwrap());

            let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

            assert_eq!(
                error,
                ConfigError::CoreSkewOutOfRange {
                    direction: Direction::Long,
                    core_skew: Rate::parse(value).unwrap(),
                },
                "{value}"
            );
        }
        assert_eq!(
            ConfigError::CoreSkewOutOfRange {
                direction: Direction::Long,
                core_skew: Rate::parse("200").unwrap(),
            }
            .to_string(),
            "做多侧：主仓倾斜须在 0.01 到 100 之间（现在是 200）"
        );
    }

    /// 旧配置的 JSON 里没有这个字段，读出来是不倾斜；写了的按字符串往返。
    #[test]
    fn the_core_skew_is_optional_in_json_and_round_trips() {
        let json = serde_json::to_value(core_long_config()).unwrap();
        let mut without = json.clone();
        without["long"].as_object_mut().unwrap().remove("core_skew");
        let parsed: StrategyConfig = serde_json::from_value(without).unwrap();
        assert_eq!(parsed, core_long_config());
        assert_eq!(json["long"]["core_skew"], serde_json::Value::Null);

        let skewed = skewed_core_long_config();
        let json = serde_json::to_value(&skewed).unwrap();
        assert_eq!(json["long"]["core_skew"], serde_json::json!("3"));
        let parsed: StrategyConfig = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, skewed);
    }

    #[test]
    fn ignores_the_core_skew_when_the_core_ratio_is_zero() {
        let mut raw = long_grid_config();
        long_side_mut(&mut raw).core_skew = Some(Rate::parse("3").unwrap());

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert_eq!(long_side(&normalized.config).core_skew, None);
    }

    /// 倾斜把首批压得太小时，报的还是「主仓不够最低金额」，并指向倾斜。
    #[test]
    fn a_steep_core_skew_can_leave_the_first_batch_below_the_minimum_order_amount() {
        let mut raw = skewed_core_long_config();
        long_side_mut(&mut raw).max_position = Quantity::parse("0.004").unwrap();
        long_side_mut(&mut raw).core_ratio = Rate::parse("0.5").unwrap();
        long_side_mut(&mut raw).core_skew = Some(Rate::parse("100").unwrap());

        let error = raw.normalize(&fine_spec(), &btc_fees()).unwrap_err();

        assert!(
            matches!(
                error,
                ConfigError::BatchCoreBelowMinimumOrderNotional {
                    batch_index: BatchIndex(0),
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn rejects_a_batch_whose_core_buy_is_below_the_minimum_order_amount() {
        // 每批 0.001，主仓部分是 0.0001，95000 的批次价下只有 9.5 美元。
        let raw = with_long(SideConfig {
            core_ratio: Rate::parse("0.1").unwrap(),
            batch_count: Some(4),
            core_skew: None,
            max_position: Quantity::parse("0.004").unwrap(),
            ..config("75000", "95000", "0.004").long.unwrap()
        });

        let error = raw.normalize(&fine_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::BatchCoreBelowMinimumOrderNotional {
                direction: Direction::Long,
                batch_index: BatchIndex(0),
                batch_price: Price::parse("95000").unwrap(),
                core_quantity: Quantity::parse("0.0001").unwrap(),
                required_quantity: Quantity::parse("0.00011").unwrap(),
            }
        );
        assert_eq!(
            error.to_string(),
            "做多侧：第 0 批（95000）主仓只有 0.0001，不够最低下单金额（至少要 0.00011）；减少批数 n，或让主仓倾斜更接近 1"
        );

        // 同样的比例与批数下，总量大一些每批的主仓就够最低金额了。
        let mut larger = raw.clone();
        long_side_mut(&mut larger).max_position = Quantity::parse("0.05").unwrap();
        assert!(larger.normalize(&fine_spec(), &btc_fees()).is_ok());
    }

    #[test]
    fn b23_rejects_cell_below_minimum_order_notional_and_reports_max_cell_count() {
        let raw = config("90000", "95000", "0.005");

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::CellBelowMinimumOrderNotional {
                direction: Direction::Long,
                cell_index: CellIndex(4),
                cell_price: Price::parse("91000").unwrap(),
                cell_quantity: Quantity::parse("0.001").unwrap(),
                required_quantity: Quantity::parse("0.003").unwrap(),
                max_cell_count: 1,
            }
        );
        // 页面原样显示：说清哪一侧、哪一格、差多少，以及怎么改。
        assert_eq!(
            error.to_string(),
            "做多侧：格子太多，每格数量不够最低下单金额（多4 格在 91000 只有 0.001，至少要 0.003）。\
             这个范围最多约 1 格：加大网格间距 d 或最大仓位 M"
        );
    }

    /// 空头格的最低金额按买入平仓价校验，不是开空价（第 2.3 节、第 13 节 B26）。
    #[test]
    fn b26_checks_the_short_cell_notional_at_the_buy_back_price() {
        let raw = StrategyConfig {
            long: None,
            short: Some(SideConfig {
                price_low: Price::parse("100").unwrap(),
                price_high: Price::parse("101").unwrap(),
                max_position: Quantity::parse("0.310").unwrap(),
                core_ratio: Rate::ZERO,
                batch_count: None,
                core_skew: None,
                grid_spacing: Price::parse("1").unwrap(),
                grid_spread: Price::parse("68").unwrap(),
                throttle_interval: None,
                invalidation: None,
            }),
            ..long_grid_config()
        };

        // 一格：开空价 100，买平价 32。整格 .310 在开空价上是 31 美元、够最低金额，
        // 在买平价上只有 9.92 美元、不够。
        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::CellBelowMinimumOrderNotional {
                direction: Direction::Short,
                cell_index: CellIndex(0),
                cell_price: Price::parse("32").unwrap(),
                cell_quantity: Quantity::parse("0.310").unwrap(),
                required_quantity: Quantity::parse("0.94").unwrap(),
                max_cell_count: 0,
            }
        );
    }

    #[test]
    fn b23_rejects_grid_spread_that_is_not_profitable_after_taker_fees() {
        let mut raw = long_grid_config();
        long_side_mut(&mut raw).grid_spread = Price::parse("1").unwrap();

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::GridSpreadNotProfitable {
                direction: Direction::Long,
                grid_spread: Price::parse("1").unwrap(),
                minimum_grid_spread: Price::parse("86").unwrap(),
                taker_fee_rate: Rate::parse("0.00045").unwrap(),
            }
        );
    }

    #[test]
    fn b23_accepts_a_grid_that_survives_taker_fees_on_both_legs() {
        let mut raw = long_grid_config();
        long_side_mut(&mut raw).grid_spread = Price::parse("86").unwrap();

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert_eq!(
            long_plan(&normalized).grid().cells()[0].exit_price,
            Price::parse("95086").unwrap()
        );
    }

    #[test]
    fn rejects_configs_that_break_the_grid_shape() {
        let spec = btc_spec();
        let fees = btc_fees();

        let mut inverted = long_grid_config();
        long_side_mut(&mut inverted).price_low = Price::parse("95000").unwrap();
        long_side_mut(&mut inverted).price_high = Price::parse("95000").unwrap();
        assert!(matches!(
            inverted.normalize(&spec, &fees).unwrap_err(),
            ConfigError::PriceRangeNotAscending { .. }
        ));

        // 规整到 0：低于半个报价单位，网格无法比一个报价单位更密。
        let mut dense = long_grid_config();
        long_side_mut(&mut dense).grid_spacing = Price::parse("0.4").unwrap();
        assert!(matches!(
            dense.normalize(&spec, &fees).unwrap_err(),
            ConfigError::GridSpacingBelowOneTick { .. }
        ));

        let mut narrow = long_grid_config();
        long_side_mut(&mut narrow).grid_spread = Price::parse("0.4").unwrap();
        assert!(matches!(
            narrow.normalize(&spec, &fees).unwrap_err(),
            ConfigError::GridSpreadBelowOneTick { .. }
        ));

        let mut tiny = long_grid_config();
        long_side_mut(&mut tiny).max_position = Quantity::parse("0.0009").unwrap();
        assert!(matches!(
            tiny.normalize(&spec, &fees).unwrap_err(),
            ConfigError::MaxPositionBelowOneStep { .. }
        ));

        let other = StrategyConfig {
            instrument: Instrument::try_new("ETH").unwrap(),
            ..long_grid_config()
        };
        assert!(matches!(
            other.normalize(&spec, &fees).unwrap_err(),
            ConfigError::InstrumentMismatch { .. }
        ));
    }

    #[test]
    fn rejects_grids_with_more_cells_than_supported() {
        let mut raw = long_grid_config();
        long_side_mut(&mut raw).price_low = Price::parse("1").unwrap();
        long_side_mut(&mut raw).price_high = Price::parse("95000").unwrap();
        long_side_mut(&mut raw).grid_spacing = Price::parse("1").unwrap();
        long_side_mut(&mut raw).max_position = Quantity::parse("100000").unwrap();

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::TooManyCells {
                direction: Direction::Long,
                cell_count: 94_999,
                max_cell_count: MAX_CELL_COUNT,
            }
        );
    }

    /// 跨过 10 万价位的网格：步长在 10 万处从 1 变成 10。买价是最高价一格格减下来的，
    /// 卖价则在 10 万下面一点点、加上价差就跨到了粗步长那一档。
    #[test]
    fn a_grid_spanning_a_hundred_thousand_normalizes_to_prices_the_exchange_takes() {
        let spec = hl_btc_spec();
        let raw = hl_config("90000", "105000", "2500", "250", "1");

        let normalized = raw.normalize(&spec, &btc_fees()).unwrap();

        let cells = long_plan(&normalized).grid().cells();
        assert_eq!(cells.len(), 6);
        assert_eq!(cells[0].entry_price, Price::parse("105000").unwrap());
        assert_eq!(cells[2].entry_price, Price::parse("100000").unwrap());
        assert_eq!(cells[5].entry_price, Price::parse("92500").unwrap());
        // 两边都跨过了 10 万：一档在它下面，一档在它上面。
        assert!(
            cells
                .iter()
                .any(|cell| cell.entry_price < Price::parse("100000").unwrap())
        );
        assert!(
            cells
                .iter()
                .any(|cell| cell.entry_price > Price::parse("100000").unwrap())
        );
        for cell in cells {
            for price in [cell.entry_price, cell.exit_price] {
                assert!(
                    is_quotable(&spec, price),
                    "第 {} 格的价格 {price} 交易所不收（该价位步长 {}，有效数字 {}）",
                    cell.index.0,
                    spec.price_rule().step_at(price),
                    significant_figures(price)
                );
            }
        }
    }

    /// 卖价跨到粗步长那一档时向上规整：价差只会变大，不会让价格变得不合规。
    #[test]
    fn a_sell_price_crossing_into_a_coarser_decade_is_rounded_up() {
        let spec = hl_btc_spec();
        let raw = hl_config("99000", "99999", "500", "95", "1");

        let normalized = raw.normalize(&spec, &btc_fees()).unwrap();

        let cells = long_plan(&normalized).grid().cells();
        assert_eq!(cells.len(), 2);
        // 99999 + 95 = 100094，在 10 万上面只能报 10 的整数倍，向上取到 100100。
        assert_eq!(cells[0].entry_price, Price::parse("99999").unwrap());
        assert_eq!(cells[0].exit_price, Price::parse("100100").unwrap());
        // 10 万下面还是 1 的整数倍，价差原样落地。
        assert_eq!(cells[1].entry_price, Price::parse("99499").unwrap());
        assert_eq!(cells[1].exit_price, Price::parse("99594").unwrap());
        assert!(is_quotable(&spec, cells[0].exit_price));
    }

    /// 空头侧从 `L` 往上排，跨进更粗的档位之后每一格都要落在**它自己那一档**的步长上，
    /// 否则挂出去会被交易所拒掉，这一格永远不成交，后面的格子也等不到放行。
    ///
    /// 评审给的参数：5 位有效数字、步长下限 0.0001、`[9.9999, 10.1]`、`d = .01`、
    /// `g = .1`、`M = 100`、数量步长 .01。第二格的理想位置 10.0099 在 10 那一档上
    /// 不是 `0.001` 的整数倍，规整之后是 10.0090。
    #[test]
    fn b10_the_short_grid_across_a_decade_normalizes_to_prices_the_exchange_takes() {
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
        let raw = StrategyConfig {
            long: None,
            short: Some(SideConfig {
                price_low: Price::parse("9.9999").unwrap(),
                price_high: Price::parse("10.1").unwrap(),
                max_position: Quantity::parse("100").unwrap(),
                core_ratio: Rate::ZERO,
                batch_count: None,
                core_skew: None,
                grid_spacing: Price::parse("0.01").unwrap(),
                grid_spread: Price::parse("0.1").unwrap(),
                throttle_interval: None,
                invalidation: None,
            }),
            ..long_grid_config()
        };

        let normalized = raw.normalize(&spec, &btc_fees()).unwrap();

        let cells = normalized
            .sides
            .iter()
            .find(|side| side.direction() == Direction::Short)
            .expect("the short side is configured")
            .grid()
            .cells();
        assert_eq!(cells.len(), 11);
        assert_eq!(cells[0].entry_price, Price::parse("9.9999").unwrap());
        assert_eq!(cells[1].entry_price, Price::parse("10.0090").unwrap());
        assert_eq!(cells[10].entry_price, Price::parse("10.0990").unwrap());
        for cell in cells {
            for price in [cell.entry_price, cell.exit_price] {
                assert!(
                    is_quotable(&spec, price),
                    "第 {} 格的价格 {price} 交易所不收（该价位步长 {}，有效数字 {}）",
                    cell.index.0,
                    spec.price_rule().step_at(price),
                    significant_figures(price)
                );
            }
            // 平仓价在建仓价下方一个价差，规整方向朝下：价差只会变大。
            assert!(
                cell.exit_price
                    <= cell
                        .entry_price
                        .saturating_sub(Price::parse("0.1").unwrap())
            );
        }
        assert!(
            cells
                .windows(2)
                .all(|pair| pair[0].entry_price < pair[1].entry_price),
            "规整之后建仓价仍然要严格递增"
        );
    }

    #[test]
    fn maximum_cell_count_is_the_largest_grid_that_still_meets_the_minimum() {
        let spec = btc_spec();
        let raw = config("90000", "95000", "0.005");

        let ConfigError::CellBelowMinimumOrderNotional { max_cell_count, .. } =
            raw.normalize(&spec, &btc_fees()).unwrap_err()
        else {
            panic!("expected a minimum order amount error");
        };

        assert_eq!(max_cell_count, 1);
        let mut candidate = raw.clone();
        long_side_mut(&mut candidate).price_low = Price::parse("94000").unwrap();
        assert!(candidate.normalize(&spec, &btc_fees()).is_ok());

        let required = required_cell_quantity(Price::parse("91000").unwrap(), &spec);
        let threshold = Notional::parse("212").unwrap();
        assert!(Notional::of(required, Price::parse("91000").unwrap()) >= threshold);
        assert!(
            Notional::of(
                Quantity::from_units(required.units() - spec.quantity_step().units()),
                Price::parse("91000").unwrap()
            ) < threshold
        );
    }

    /// 有主仓时格子只分到网格那部分（M × (1 − r)），格数上限要按它算：按整份 M 算会报出
    /// 偏大的格数，照着改还是通不过。
    #[test]
    fn b23_the_max_cell_count_leaves_out_the_core() {
        let with_spacing = |spacing: &str| StrategyConfig {
            long: Some(SideConfig {
                core_ratio: Rate::parse("0.5").unwrap(),
                batch_count: Some(1),
                core_skew: None,
                grid_spacing: Price::parse(spacing).unwrap(),
                ..config("85000", "95000", "0.02").long.unwrap()
            }),
            ..config("85000", "95000", "0.02")
        };

        let ConfigError::CellBelowMinimumOrderNotional { max_cell_count, .. } =
            with_spacing("1000")
                .normalize(&btc_spec(), &btc_fees())
                .unwrap_err()
        else {
            panic!("expected a minimum order amount error");
        };

        // 网格那部分只有 0.01，每格至少要 0.003：放得下 3 格，放不下 4 格。按整份 M 算会报 6 格。
        assert_eq!(max_cell_count, 3);
        assert!(
            with_spacing("3334")
                .normalize(&btc_spec(), &btc_fees())
                .is_ok()
        );
        assert!(
            with_spacing("2500")
                .normalize(&btc_spec(), &btc_fees())
                .is_err()
        );
    }

    /// 空头侧的格数上限要逐格检查：数量最小的一格在价格最高处，最低价那一格反而是
    /// 数量最大的一格，只看最低价那一格会报出偏小的格数。
    #[test]
    fn b26_the_short_max_cell_count_checks_every_cell() {
        let short = |low: &str, high: &str| StrategyConfig {
            long: None,
            short: Some(SideConfig {
                price_low: Price::parse(low).unwrap(),
                price_high: Price::parse(high).unwrap(),
                max_position: Quantity::parse("0.05").unwrap(),
                core_ratio: Rate::ZERO,
                batch_count: None,
                core_skew: None,
                grid_spacing: Price::parse("1000").unwrap(),
                grid_spread: Price::parse("1000").unwrap(),
                throttle_interval: None,
                invalidation: None,
            }),
            ..long_grid_config()
        };

        // 五格：每格 .010，最低的那一格买平价 2000、金额只有 20 美元，不够。
        let ConfigError::CellBelowMinimumOrderNotional { max_cell_count, .. } =
            short("3000", "8000")
                .normalize(&btc_spec(), &btc_fees())
                .unwrap_err()
        else {
            panic!("expected a minimum order amount error");
        };

        // 三格放得下：每格 .017/.017/.016，最低价那格 34 美元，正好够。
        assert_eq!(max_cell_count, 3);
        assert!(
            short("3000", "6000")
                .normalize(&btc_spec(), &btc_fees())
                .is_ok()
        );
        // 四格放不下：每格 .013，最低价那格只有 26 美元。
        assert!(
            short("3000", "7000")
                .normalize(&btc_spec(), &btc_fees())
                .is_err()
        );
    }

    /// 限速间隔是配置的一部分：规整不改动它，写进数据库再读回来还是同一个值。
    #[test]
    fn b44_the_throttle_interval_survives_normalizing_and_json() {
        let mut raw = core_config();
        long_side_mut(&mut raw).throttle_interval = Some(Duration::minutes(60));

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert_eq!(
            long_side(&normalized.config).throttle_interval,
            Some(Duration::minutes(60))
        );
        let json = serde_json::to_string(&normalized.config).unwrap();
        let restored: StrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, normalized.config);
        assert_eq!(
            long_side(&restored).throttle_interval,
            Some(Duration::minutes(60))
        );

        // 不限速也是配置的一部分：`None` 不会在读回来之后变成别的值。
        let plain = long_grid_config()
            .normalize(&btc_spec(), &btc_fees())
            .unwrap();
        let restored: StrategyConfig =
            serde_json::from_str(&serde_json::to_string(&plain.config).unwrap()).unwrap();
        assert_eq!(long_side(&restored).throttle_interval, None);
    }

    #[test]
    fn rejects_a_negative_throttle_interval() {
        let mut raw = core_config();
        long_side_mut(&mut raw).throttle_interval = Some(Duration::seconds(-1));

        assert_eq!(
            raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::NegativeThrottleInterval {
                direction: Direction::Long,
                interval: Duration::seconds(-1),
            }
        );

        // 零是允许的：相邻两个单位仍然要等前一个单位先成交。
        long_side_mut(&mut raw).throttle_interval = Some(Duration::zero());
        assert!(raw.normalize(&btc_spec(), &btc_fees()).is_ok());
    }

    /// 两侧价格范围必须留出空隙：多头侧最高价严格低于空头侧最低价。
    #[test]
    fn b25_rejects_sides_whose_price_ranges_overlap_or_touch() {
        // 多头侧 75000–95000，空头侧 90000–99000：中间重叠。
        let overlapping = StrategyConfig {
            long: Some(config("75000", "95000", "1").long.unwrap()),
            short: Some(config("90000", "99000", "1").long.unwrap()),
            ..long_grid_config()
        };
        assert_eq!(
            overlapping.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::SidesOverlap {
                long_high: Price::parse("95000").unwrap(),
                short_low: Price::parse("90000").unwrap(),
            }
        );

        // 首尾相接也不行：95000 与 95000 之间没有空隙，两侧的挂单会落在同一个价位上。
        let touching = StrategyConfig {
            long: Some(config("75000", "95000", "1").long.unwrap()),
            short: Some(config("95000", "99000", "1").long.unwrap()),
            ..long_grid_config()
        };
        assert!(matches!(
            touching.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::SidesOverlap { .. }
        ));

        // 留出一格空隙就是合法的中性网格。
        let apart = StrategyConfig {
            long: Some(config("75000", "95000", "1").long.unwrap()),
            short: Some(config("96000", "99000", "1").long.unwrap()),
            ..long_grid_config()
        };
        let normalized = apart.normalize(&btc_spec(), &btc_fees()).unwrap();
        assert_eq!(normalized.sides.len(), 2);
        assert_eq!(normalized.sides[0].direction(), Direction::Long);
        assert_eq!(normalized.sides[1].direction(), Direction::Short);
    }

    /// 中性网格没有主仓：两侧都配置时任何一侧设了 `r > 0` 都拒绝。
    #[test]
    fn b25_rejects_a_core_ratio_in_a_neutral_grid() {
        let neutral = |long: SideConfig, short: SideConfig| StrategyConfig {
            long: Some(long),
            short: Some(short),
            ..long_grid_config()
        };
        let plain =
            |low: &str, high: &str, position: &str| config(low, high, position).long.unwrap();
        let with_core = |low: &str, high: &str, position: &str| SideConfig {
            core_ratio: Rate::parse("0.7").unwrap(),
            batch_count: Some(4),
            core_skew: None,
            ..config(low, high, position).long.unwrap()
        };
        // 空头侧的范围要够宽：四批里离 `H` 最近的那一批，段宽是 `(H − L) / n`，
        // 窄于一个间距就没有格子跟随它。
        let long_side = || plain("75000", "95000", "1");
        let short_side = || plain("96000", "106000", "1");

        // 多头侧有主仓。
        assert_eq!(
            neutral(with_core("75000", "95000", "1"), short_side())
                .normalize(&btc_spec(), &btc_fees())
                .unwrap_err(),
            ConfigError::CoreInNeutralGrid {
                direction: Direction::Long,
                core_ratio: Rate::parse("0.7").unwrap(),
            }
        );

        // 空头侧有主仓：报出的是空头侧。
        assert_eq!(
            neutral(long_side(), with_core("96000", "106000", "1"))
                .normalize(&btc_spec(), &btc_fees())
                .unwrap_err(),
            ConfigError::CoreInNeutralGrid {
                direction: Direction::Short,
                core_ratio: Rate::parse("0.7").unwrap(),
            }
        );

        // 两侧都没有主仓才是中性网格。
        assert!(
            neutral(long_side(), short_side())
                .normalize(&btc_spec(), &btc_fees())
                .is_ok()
        );
    }

    /// 两侧都不配置时没有策略可跑。
    #[test]
    fn rejects_a_config_without_any_side() {
        let empty = StrategyConfig {
            long: None,
            short: None,
            ..long_grid_config()
        };

        assert_eq!(
            empty.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::NoSidesConfigured
        );
    }

    fn invalidation(price: &str, remaining: &str) -> Option<super::Invalidation> {
        Some(super::Invalidation {
            price: Price::parse(price).unwrap(),
            remaining: Quantity::parse(remaining).unwrap(),
            max_slippage: Rate::parse("0.01").unwrap(),
        })
    }

    fn core_long_with(invalidation: Option<super::Invalidation>) -> StrategyConfig {
        let mut config = crate::fixtures::core_long_config();
        long_side_mut(&mut config).invalidation = invalidation;
        config
    }

    /// B41：多头侧失效价不低于 L 时拒绝。
    #[test]
    fn b41_a_long_invalidation_price_inside_the_range_is_refused() {
        let error = core_long_with(invalidation("75000", "0"))
            .normalize(&btc_spec(), &btc_fees())
            .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidationPriceInsideRange { .. }
        ));
    }

    /// B41：空头侧失效价不高于 H 时拒绝。
    #[test]
    fn b41_a_short_invalidation_price_inside_the_range_is_refused() {
        let mut config = crate::fixtures::core_short_config();
        config.short.as_mut().unwrap().invalidation = invalidation("95000", "0");

        let error = config.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidationPriceInsideRange { .. }
        ));
    }

    /// B41：剩余数量超过本侧主仓总量（.175 × 4 = .7）时拒绝。
    #[test]
    fn b41_a_remaining_above_the_core_total_is_refused() {
        let error = core_long_with(invalidation("70000", "0.701"))
            .normalize(&btc_spec(), &btc_fees())
            .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidationRemainingAboveCore { .. }
        ));
    }

    /// 合法的失效设置原样保留，剩余数量按数量精度向下规整。
    #[test]
    fn a_valid_invalidation_is_kept_with_the_remaining_rounded_down() {
        let normalized = core_long_with(invalidation("70000", "0.3009"))
            .normalize(&btc_spec(), &btc_fees())
            .unwrap();

        let kept = long_side(&normalized.config).invalidation.unwrap();
        assert_eq!(kept.price, Price::parse("70000").unwrap());
        assert_eq!(kept.remaining, Quantity::parse("0.3").unwrap());
    }

    /// 中性网格任一侧失效都全部平仓，剩余数量只能是 0。
    #[test]
    fn a_neutral_grid_must_close_everything_on_invalidation() {
        let mut config = crate::fixtures::neutral_grid_config();
        config.long.as_mut().unwrap().invalidation = invalidation("78", "0.1");

        let error = config
            .normalize(&crate::fixtures::cheap_spec(), &btc_fees())
            .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidationRemainingInNeutralGrid { .. }
                | ConfigError::InvalidationRemainingAboveCore { .. }
        ));
    }

    /// 滑点上限要在 0 和 100% 之间。
    #[test]
    fn the_invalidation_slippage_must_be_positive_and_below_one() {
        let mut bad = invalidation("70000", "0");
        bad.as_mut().unwrap().max_slippage = Rate::ZERO;

        let error = core_long_with(bad)
            .normalize(&btc_spec(), &btc_fees())
            .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidationSlippageOutOfRange { .. }
        ));
    }
}
