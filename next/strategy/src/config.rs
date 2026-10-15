//! 策略配置与配置校验。
//!
//! 校验都使用规整后的值：价格按报价精度规整，数量按数量精度向下规整。

use chrono::Duration;
use serde::{Deserialize, Serialize};

use crate::batch::{Batch, BatchIndex, BatchPlan, batch_price};
use crate::grid::{Cell, CellIndex, GridPlan, cell_count, cell_prices};
use crate::instrument::{Fees, Instrument, InstrumentSpec};
use crate::units::{Notional, Price, PriceRounding, Quantity, Rate, SCALE, div_ceil, split_steps};

/// 网格格数的技术上限。
///
/// 设计没有规定这个上限。它只用来挡住明显不合理的配置（例如范围极大、间距极小），
/// 正常配置远达不到；按 Hyperliquid 每日 5 万次操作上限，格数超过这个量级本身也不可用。
pub const MAX_CELL_COUNT: usize = 10_000;

/// 多头侧的配置。
///
/// 观点失效价属于后续任务，这里还没有对应的字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideConfig {
    /// 本侧建仓的价格范围下限 `L`。
    pub price_low: Price,
    /// 本侧建仓的价格范围上限 `H`。
    pub price_high: Price,
    /// 本侧最多持有的数量 `M`。
    pub max_position: Quantity,
    /// 主仓比例 `r`，取值 `[0, 1]`；0 表示没有主仓，格子自成单位，1 表示只积累主仓、没有网格。
    pub core_ratio: Rate,
    /// 主仓批数 `n`；`r = 0` 时不需要，写 `None`。
    pub batch_count: Option<u32>,
    /// 网格间距 `d`。
    pub grid_spacing: Price,
    /// 网格价差 `g`：每格卖价 = 格子买价 + g。
    pub grid_spread: Price,
    /// 限速间隔 τ（可选，第 4.1 节）：同一侧在 τ 时间内最多启动一个单位；`None` 表示不限速。
    ///
    /// 它是运行中可以修改的参数（第 2.4 节），所以在配置里而不是状态里。
    pub throttle_interval: Option<Duration>,
}

/// 做多网格配置：只有多头侧，不限速。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub instrument: Instrument,
    pub long: SideConfig,
}

/// 规整后的配置、批次划分与格子划分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NormalizedConfig {
    pub config: StrategyConfig,
    pub batches: BatchPlan,
    pub grid: GridPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("config instrument `{config}` does not match the exchange instrument `{spec}`")]
    InstrumentMismatch { config: String, spec: String },
    #[error("{field} must not be negative, got {price}")]
    NegativePrice { field: &'static str, price: Price },
    #[error("max position must not be negative, got {max_position}")]
    NegativeMaxPosition { max_position: Quantity },
    #[error("price range must be ascending after rounding, got {low} and {high}")]
    PriceRangeNotAscending { low: Price, high: Price },
    #[error("{field} {price} is out of the representable price range at tick {price_tick}")]
    PriceOutOfRange {
        field: &'static str,
        price: Price,
        price_tick: Price,
    },
    #[error(
        "batch {batch_index} price between {price_low} and {price_high} is out of the representable price range"
    )]
    BatchPriceOutOfRange {
        batch_index: BatchIndex,
        price_low: Price,
        price_high: Price,
    },
    #[error("grid spacing {grid_spacing} must be at least one price tick {price_tick}")]
    GridSpacingBelowOneTick {
        grid_spacing: Price,
        price_tick: Price,
    },
    #[error("grid spread {grid_spread} must be at least one price tick {price_tick}")]
    GridSpreadBelowOneTick {
        grid_spread: Price,
        price_tick: Price,
    },
    #[error("max position {max_position} is below one quantity step {quantity_step}")]
    MaxPositionBelowOneStep {
        max_position: Quantity,
        quantity_step: Quantity,
    },
    #[error("core ratio {core_ratio} must be between 0 and 1 inclusive")]
    CoreRatioOutOfRange { core_ratio: Rate },
    #[error("throttle interval must not be negative, got {interval:?}")]
    NegativeThrottleInterval { interval: Duration },
    #[error("core ratio is above 0, so the batch count must be at least 1, got {batch_count:?}")]
    BatchCountOutOfRange { batch_count: Option<u32> },
    #[error("batch count {batch_count} is above the supported maximum {max_batch_count}")]
    TooManyBatches {
        batch_count: u32,
        max_batch_count: usize,
    },
    #[error("grid needs {cell_count} cells, more than the supported maximum {max_cell_count}")]
    TooManyCells {
        cell_count: usize,
        max_cell_count: usize,
    },
    #[error(
        "cell {cell_index} at {cell_price} holds {cell_quantity}, below the {required_quantity} required for the minimum order amount; at most {max_cell_count} cells fit in this range"
    )]
    CellBelowMinimumOrderNotional {
        cell_index: CellIndex,
        cell_price: Price,
        cell_quantity: Quantity,
        required_quantity: Quantity,
        max_cell_count: usize,
    },
    #[error(
        "batch {batch_index} at {batch_price} has no cell in its segment ({segment_low}, {segment_high}]; use a smaller grid spacing or fewer batches"
    )]
    BatchWithoutCell {
        batch_index: BatchIndex,
        batch_price: Price,
        segment_low: Price,
        segment_high: Price,
    },
    #[error(
        "batch {batch_index} at {batch_price} buys {core_quantity} as core, below the {required_quantity} required for the minimum order amount; use fewer batches"
    )]
    BatchCoreBelowMinimumOrderNotional {
        batch_index: BatchIndex,
        batch_price: Price,
        core_quantity: Quantity,
        required_quantity: Quantity,
    },
    #[error(
        "cell {cell_index} at {buy_price} plus the grid spread {grid_spread} is a price out of range"
    )]
    SellPriceOutOfRange {
        cell_index: CellIndex,
        buy_price: Price,
        grid_spread: Price,
    },
    #[error(
        "grid spread {grid_spread} does not stay profitable after taker fees {taker_fee_rate} on both legs; at least {minimum_grid_spread} is required"
    )]
    GridSpreadNotProfitable {
        grid_spread: Price,
        minimum_grid_spread: Price,
        taker_fee_rate: Rate,
    },
    #[error(
        "no price at {buy_price} can cover taker fees {taker_fee_rate} on both legs within the representable range"
    )]
    GridSpreadUnreachable {
        buy_price: Price,
        taker_fee_rate: Rate,
    },
    #[error("taker fee rate {taker_fee_rate} is at least 1, so no grid can be profitable")]
    TakerFeeRateTooHigh { taker_fee_rate: Rate },
}

impl StrategyConfig {
    /// 规整配置、划分批次与格子并校验。
    pub(crate) fn normalize(
        &self,
        spec: &InstrumentSpec,
        fees: &Fees,
    ) -> Result<NormalizedConfig, ConfigError> {
        if &self.instrument != spec.instrument() {
            return Err(ConfigError::InstrumentMismatch {
                config: self.instrument.symbol().to_string(),
                spec: spec.instrument().symbol().to_string(),
            });
        }

        // 符号检查放在规整之前：规整只对非负值有意义，负价格、负间距、负数量
        // 应该在这里变成配置错误，而不是走到规整里的断言上。
        self.check_signs()?;

        let price_low = normalize_price("price low", self.long.price_low, spec)?;
        let price_high = normalize_price("price high", self.long.price_high, spec)?;
        if price_low >= price_high {
            return Err(ConfigError::PriceRangeNotAscending {
                low: price_low,
                high: price_high,
            });
        }

        let price_tick = spec.price_tick();
        let grid_spacing = normalize_price("grid spacing", self.long.grid_spacing, spec)?;
        let grid_spread = normalize_price("grid spread", self.long.grid_spread, spec)?;

        let max_position = spec.normalize_quantity(self.long.max_position);
        if max_position.units() < spec.quantity_step().units() {
            return Err(ConfigError::MaxPositionBelowOneStep {
                max_position,
                quantity_step: spec.quantity_step(),
            });
        }

        let core_ratio = self.long.core_ratio;
        if core_ratio.units() > SCALE {
            return Err(ConfigError::CoreRatioOutOfRange { core_ratio });
        }
        // 主仓比例为 0 时没有批次，批数不参与计算，写什么都不影响结果。
        let batch_count = match self.long.batch_count {
            _ if core_ratio.is_zero() => None,
            Some(count) if count >= 1 => Some(count),
            other => return Err(ConfigError::BatchCountOutOfRange { batch_count: other }),
        };

        // 主仓比例为 1 时只积累主仓，没有网格（第 2.3 节）：不生成格子，网格间距、
        // 价差和格子金额都不需要，也就不校验；只校验每批的主仓买入金额。
        let core_only = core_ratio.units() == SCALE;
        let prices = if core_only {
            Vec::new()
        } else {
            if grid_spacing.units() < price_tick.units() {
                return Err(ConfigError::GridSpacingBelowOneTick {
                    grid_spacing,
                    price_tick,
                });
            }
            if grid_spread.units() < price_tick.units() {
                return Err(ConfigError::GridSpreadBelowOneTick {
                    grid_spread,
                    price_tick,
                });
            }
            // 先按范围与间距算出格数，再构造价格：极端配置不能因为分配内存而拖死进程。
            let count = cell_count(price_high, price_low, grid_spacing);
            if count > MAX_CELL_COUNT as i64 {
                return Err(ConfigError::TooManyCells {
                    cell_count: count as usize,
                    max_cell_count: MAX_CELL_COUNT,
                });
            }
            cell_prices(price_high, price_low, grid_spacing)
        };
        // 先挡住过大的批数再划分批次：极端配置不能因为分配内存而拖死进程，
        // 具体的「哪一批没有格子」由划分批次时报出。
        if let Some(batch_count) = batch_count
            && batch_count as usize > MAX_CELL_COUNT
        {
            return Err(ConfigError::TooManyBatches {
                batch_count,
                max_batch_count: MAX_CELL_COUNT,
            });
        }
        let shape = GridShape {
            price_low,
            price_high,
            max_position,
            core_ratio,
            grid_spread,
        };

        let (batches, cells) = match batch_count {
            Some(batch_count) => build_batches(&prices, &shape, batch_count, spec)?,
            None => (BatchPlan::default(), build_cells(&prices, &shape, spec)?),
        };

        // 最低金额：每一格按两条腿中较低的价格计算。多头侧的最低腿是格子买价，
        // 价格最低的一格最严格；这里逐格检查，不依赖这条推理。
        // 没有网格时 `cells` 是空的，这些检查自然跳过。
        for cell in cells.iter().rev() {
            if !meets_minimum_cell_notional(cell.quantity, cell.buy_price, spec) {
                return Err(ConfigError::CellBelowMinimumOrderNotional {
                    cell_index: cell.index,
                    cell_price: cell.buy_price,
                    cell_quantity: cell.quantity,
                    required_quantity: required_cell_quantity(cell.buy_price, spec),
                    max_cell_count: max_cell_count(
                        cells.len(),
                        price_high,
                        max_position,
                        spec,
                        grid_spacing,
                    ),
                });
            }
        }

        // 扣费后盈利：每格的价差要覆盖两条腿的 taker 费率。价格最高的一格最严格。
        for cell in cells.iter() {
            if !spread_is_profitable(cell.buy_price, cell.sell_price, fees.taker_rate) {
                let minimum_grid_spread =
                    minimum_grid_spread(cell.buy_price, fees.taker_rate, price_tick)?;
                return Err(ConfigError::GridSpreadNotProfitable {
                    grid_spread,
                    minimum_grid_spread,
                    taker_fee_rate: fees.taker_rate,
                });
            }
        }

        Ok(NormalizedConfig {
            config: StrategyConfig {
                instrument: self.instrument.clone(),
                long: SideConfig {
                    price_low,
                    price_high,
                    max_position,
                    core_ratio,
                    batch_count,
                    grid_spacing,
                    grid_spread,
                    throttle_interval: self.long.throttle_interval,
                },
            },
            batches,
            grid: GridPlan::from_cells(cells),
        })
    }

    /// 规整之前先看符号：价格、数量、比例都不接受负值。
    ///
    /// 规整函数只对非负值有意义，先检查才能把普通非法输入变成可处理的
    /// [`ConfigError`]，而不是让调用方撞上规整里的断言。
    fn check_signs(&self) -> Result<(), ConfigError> {
        let long = &self.long;
        for (field, price) in [
            ("price low", long.price_low),
            ("price high", long.price_high),
            ("grid spacing", long.grid_spacing),
            ("grid spread", long.grid_spread),
        ] {
            if price.units() < 0 {
                return Err(ConfigError::NegativePrice { field, price });
            }
        }
        if long.max_position.units() < 0 {
            return Err(ConfigError::NegativeMaxPosition {
                max_position: long.max_position,
            });
        }
        if long.core_ratio.units() < 0 {
            return Err(ConfigError::CoreRatioOutOfRange {
                core_ratio: long.core_ratio,
            });
        }
        // 限速间隔只用来算「下一笔放行还要等多久」，负值没有意义。
        if let Some(interval) = long.throttle_interval
            && interval < Duration::zero()
        {
            return Err(ConfigError::NegativeThrottleInterval { interval });
        }
        Ok(())
    }
}

/// 按报价精度规整；规整结果超出可表示范围时是配置错误，不做饱和。
fn normalize_price(
    field: &'static str,
    price: Price,
    spec: &InstrumentSpec,
) -> Result<Price, ConfigError> {
    spec.normalize_price(price, PriceRounding::Nearest)
        .ok_or(ConfigError::PriceOutOfRange {
            field,
            price,
            price_tick: spec.price_tick(),
        })
}

/// 规整后的网格形状：划分批次与格子时反复用到的参数。
struct GridShape {
    price_low: Price,
    price_high: Price,
    max_position: Quantity,
    core_ratio: Rate,
    grid_spread: Price,
}

/// 划分批次，并把每批的网格部分分给跟随它的格子。
///
/// `prices` 为空表示这一侧没有网格（`r = 1`）：每批只有主仓，没有格子，
/// 也就不适用「每批都要有格子」这条规则。
fn build_batches(
    prices: &[Price],
    shape: &GridShape,
    batch_count: u32,
    spec: &InstrumentSpec,
) -> Result<(BatchPlan, Vec<Cell>), ConfigError> {
    let count = batch_count as usize;
    let step = spec.quantity_step();
    let (price_high, price_low) = (shape.price_high, shape.price_low);

    // 批次价：把范围均分后按报价精度规整。
    let batch_prices: Vec<Price> = (0..count)
        .map(|index| {
            batch_price(price_high, price_low, batch_count, index, spec.price_tick()).ok_or(
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
        if !meets_minimum_core_notional(core_quantity, batch_prices[index], spec) {
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
                sell_price: sell_price(prices[cell_index.0], *cell_index, shape)?,
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

/// 把 `M` 按数量最小单位平均分给各格，余下的最小单位依次给靠近空隙的格子。
fn build_cells(
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
fn sell_price(
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

fn cell_quantity(
    index: usize,
    cell_count: usize,
    max_position: Quantity,
    step: Quantity,
) -> Quantity {
    let total_steps = max_position.units() / step.units();
    Quantity::from_units(split_steps(total_steps, index, cell_count) * step.units())
}

/// 一批的主仓数量 `C = ceil(r × q)`，按数量最小单位向上取整。
///
/// `quantity` 本身是数量精度的整数倍，所以向上规整的结果不会超过它，
/// 也就不会超出可表示范围；真越界了也夹回本批数量。
fn core_quantity(quantity: Quantity, core_ratio: Rate, step: Quantity) -> Quantity {
    quantity
        .mul_rate_ceil(core_ratio)
        .checked_ceil_to_step(step)
        .unwrap_or(quantity)
        .min(quantity)
}

/// 格子数量低于最低订单金额时，这一格至少需要多少数量。
///
/// 只用于错误信息。算出的数量超出可表示范围时按上限取值：反正这个配置本来就不合法。
fn required_cell_quantity(price: Price, spec: &InstrumentSpec) -> Quantity {
    let step_units = spec.quantity_step().units() as i128;
    let minimum_raw = 3 * spec.min_order_notional().raw();
    let price_units = (price.units() as i128).max(1);
    let units = div_ceil(minimum_raw, price_units) + 2 * step_units;
    let units = div_ceil(units, step_units) * step_units;
    Quantity::from_units(i64::try_from(units).unwrap_or(i64::MAX))
}

fn meets_minimum_cell_notional(quantity: Quantity, price: Price, spec: &InstrumentSpec) -> bool {
    let Some(three_minimum) = spec.min_order_notional().checked_mul(3) else {
        return false;
    };
    let two_steps = Notional::of(spec.quantity_step(), price)
        .checked_mul(2)
        .unwrap_or(Notional::ZERO);
    Notional::of(quantity, price) >= three_minimum + two_steps
}

/// 一批主仓买入低于最低订单金额时，这一批至少需要多少数量。
///
/// 只用于错误信息。算出的数量超出可表示范围时按上限取值：反正这个配置本来就不合法。
fn required_core_quantity(price: Price, spec: &InstrumentSpec) -> Quantity {
    let step_units = spec.quantity_step().units() as i128;
    let price_units = (price.units() as i128).max(1);
    let units = div_ceil(spec.min_order_notional().raw(), price_units);
    let units = div_ceil(units, step_units) * step_units;
    Quantity::from_units(i64::try_from(units).unwrap_or(i64::MAX))
}

fn meets_minimum_core_notional(
    core_quantity: Quantity,
    batch_price: Price,
    spec: &InstrumentSpec,
) -> bool {
    spec.meets_minimum_order(core_quantity, batch_price)
}

/// 同样的价格范围与间距下，最多能设多少格。
///
/// 格数越少，每格数量越大、最低价的格子也越高，所以最低金额约束随格数单调放松。
/// 最低价的一格总是最严格的一格，因此只看它。
fn max_cell_count(
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
fn spread_is_profitable(buy: Price, sell: Price, taker: Rate) -> bool {
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
fn minimum_grid_spread(price: Price, taker: Rate, tick: Price) -> Result<Price, ConfigError> {
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
    use super::{ConfigError, MAX_CELL_COUNT, SideConfig, StrategyConfig, required_cell_quantity};
    use crate::batch::BatchIndex;
    use crate::fixtures::{btc_fees, btc_spec, core_only_config, fine_spec, long_grid_config};
    use crate::grid::CellIndex;
    use crate::instrument::{Fees, Instrument, InstrumentSpec};
    use crate::units::{Notional, Price, Quantity, Rate};
    use chrono::Duration;

    /// 没有主仓的配置，价格范围与数量按参数给定。
    fn config(price_low: &str, price_high: &str, max_position: &str) -> StrategyConfig {
        StrategyConfig {
            instrument: Instrument::try_new("BTC").unwrap(),
            long: SideConfig {
                price_low: Price::parse(price_low).unwrap(),
                price_high: Price::parse(price_high).unwrap(),
                max_position: Quantity::parse(max_position).unwrap(),
                core_ratio: Rate::ZERO,
                batch_count: None,
                grid_spacing: Price::parse("1000").unwrap(),
                grid_spread: Price::parse("1000").unwrap(),
                throttle_interval: None,
            },
        }
    }

    /// 设计第 3.2 节的例子加上主仓：`r = .7`，`n = 4`。
    ///
    /// 每批 `q = .25`，主仓 `C = .175`，网格 `G = .075`；每批 5 格，每格 `.015`。
    fn core_config() -> StrategyConfig {
        core_config_with(4)
    }

    fn core_config_with(batch_count: u32) -> StrategyConfig {
        StrategyConfig {
            long: SideConfig {
                core_ratio: Rate::parse("0.7").unwrap(),
                batch_count: Some(batch_count),
                ..config("75000", "95000", "1").long
            },
            ..config("75000", "95000", "1")
        }
    }

    #[test]
    fn normalizes_prices_to_tick_and_quantity_down_to_step() {
        let mut raw = long_grid_config();
        raw.long.price_low = Price::parse("75000.4").unwrap();
        raw.long.price_high = Price::parse("95000.4").unwrap();
        raw.long.max_position = Quantity::parse("1.00049").unwrap();

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert_eq!(
            normalized.config.long.price_low,
            Price::parse("75000").unwrap()
        );
        assert_eq!(
            normalized.config.long.price_high,
            Price::parse("95000").unwrap()
        );
        assert_eq!(
            normalized.config.long.max_position,
            Quantity::parse("1").unwrap()
        );
        assert_eq!(
            normalized.grid.cells()[0].buy_price,
            Price::parse("95000").unwrap()
        );
        assert_eq!(
            normalized.grid.cells()[19].buy_price,
            Price::parse("76000").unwrap()
        );
    }

    #[test]
    fn grid_cells_start_at_high_and_carry_the_spread() {
        let normalized = long_grid_config()
            .normalize(&btc_spec(), &btc_fees())
            .unwrap();

        let cells = normalized.grid.cells();
        assert_eq!(cells.len(), 20);
        assert_eq!(cells[0].buy_price, Price::parse("95000").unwrap());
        assert_eq!(cells[0].sell_price, Price::parse("96000").unwrap());
        assert_eq!(cells[19].buy_price, Price::parse("76000").unwrap());
        assert_eq!(cells[19].sell_price, Price::parse("77000").unwrap());
        assert!(
            cells
                .iter()
                .all(|cell| cell.quantity == Quantity::parse("0.05").unwrap())
        );
        assert!(cells.iter().all(|cell| cell.batch.is_none()));
        assert!(normalized.batches.is_empty());
    }

    #[test]
    fn cell_quantities_give_the_remainder_to_the_cells_nearest_the_gap() {
        let raw = config("75000", "95000", "0.063");

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        let cells = normalized.grid.cells();
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

        let batches = normalized.batches.batches();
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
        for (index, cell) in normalized.grid.cells().iter().enumerate() {
            assert_eq!(cell.batch, Some(BatchIndex(index / 5)));
            assert_eq!(cell.quantity, Quantity::parse("0.015").unwrap());
        }
    }

    #[test]
    fn b27_batch_quantities_give_the_remainder_to_the_batches_nearest_the_gap() {
        let raw = core_config_with(3);

        let normalized = raw.normalize(&fine_spec(), &btc_fees()).unwrap();

        let batches = normalized.batches.batches();
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
        let batch = &normalized.batches.batches()[0];
        assert_eq!(batch.cells.len(), 7);
        assert_eq!(batch.grid_quantity, Quantity::parse("0.1").unwrap());
        let cells = normalized.grid.cells();
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
        raw.long.grid_spacing = Price::parse("8000").unwrap();

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        // 8000 的间距下只有 95000 和 87000 两格：85 批的段里没有格子。
        assert_eq!(
            error,
            ConfigError::BatchWithoutCell {
                batch_index: BatchIndex(2),
                batch_price: Price::parse("85000").unwrap(),
                segment_low: Price::parse("80000").unwrap(),
                segment_high: Price::parse("85000").unwrap(),
            }
        );
        assert!(error.to_string().contains("smaller grid spacing"));

        // 缩小网格间距，每批的段内就都有格子了。
        let mut denser = raw.clone();
        denser.long.grid_spacing = Price::parse("1000").unwrap();
        assert!(denser.normalize(&btc_spec(), &btc_fees()).is_ok());
    }

    #[test]
    fn rejects_a_core_ratio_outside_zero_to_one() {
        let mut negative = core_config();
        negative.long.core_ratio = Rate::parse("-0.1").unwrap();
        assert_eq!(
            negative.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::CoreRatioOutOfRange {
                core_ratio: Rate::parse("-0.1").unwrap(),
            }
        );

        // 1 是允许的（第 2.3 节），再往上就不行。
        let mut above = core_config();
        above.long.core_ratio = Rate::parse("1.00000001").unwrap();
        assert_eq!(
            above.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::CoreRatioOutOfRange {
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
        assert!(normalized.grid.is_empty());
        let batches = normalized.batches.batches();
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
        core_only.long.grid_spacing = Price::parse("0.4").unwrap();
        core_only.long.grid_spread = Price::parse("0.4").unwrap();
        assert!(core_only.normalize(&btc_spec(), &btc_fees()).is_ok());

        let mut grid = long_grid_config();
        grid.long.grid_spacing = Price::parse("0.4").unwrap();
        assert!(matches!(
            grid.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::GridSpacingBelowOneTick { .. }
        ));
    }

    #[test]
    fn a_full_core_ratio_still_checks_the_per_batch_core_buy() {
        // 数量精度更细：每批只剩 0.0001，在 95000 的批次价下不到最低金额 10 美元。
        let mut raw = core_only_config();
        raw.long.max_position = Quantity::parse("0.0004").unwrap();

        assert_eq!(
            raw.normalize(&fine_spec(), &btc_fees()).unwrap_err(),
            ConfigError::BatchCoreBelowMinimumOrderNotional {
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
        negative_low.long.price_low = Price::parse("-1").unwrap();
        assert!(matches!(
            negative_low.normalize(&spec, &fees).unwrap_err(),
            ConfigError::NegativePrice { .. }
        ));

        let mut negative_spacing = long_grid_config();
        negative_spacing.long.grid_spacing = Price::parse("-1000").unwrap();
        assert!(matches!(
            negative_spacing.normalize(&spec, &fees).unwrap_err(),
            ConfigError::NegativePrice { .. }
        ));

        let mut negative_spread = long_grid_config();
        negative_spread.long.grid_spread = Price::parse("-1000").unwrap();
        assert!(matches!(
            negative_spread.normalize(&spec, &fees).unwrap_err(),
            ConfigError::NegativePrice { .. }
        ));

        let mut negative_quantity = long_grid_config();
        negative_quantity.long.max_position = Quantity::parse("-1").unwrap();
        assert!(matches!(
            negative_quantity.normalize(&spec, &fees).unwrap_err(),
            ConfigError::NegativeMaxPosition { .. }
        ));

        let mut negative_ratio = core_config();
        negative_ratio.long.core_ratio = Rate::parse("-0.5").unwrap();
        assert!(matches!(
            negative_ratio.normalize(&spec, &fees).unwrap_err(),
            ConfigError::CoreRatioOutOfRange { .. }
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
            let raw = StrategyConfig {
                long: SideConfig {
                    price_low: Price::parse(low).unwrap(),
                    price_high: Price::parse(high).unwrap(),
                    max_position: Quantity::parse("1").unwrap(),
                    grid_spacing: Price::parse("1").unwrap(),
                    grid_spread: Price::parse(spread).unwrap(),
                    ..long_grid_config().long
                },
                ..long_grid_config()
            };

            assert_eq!(
                raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
                ConfigError::SellPriceOutOfRange {
                    cell_index: CellIndex(0),
                    buy_price: Price::parse(high).unwrap(),
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
        let raw = StrategyConfig {
            long: SideConfig {
                price_low: Price::parse("92233720367").unwrap(),
                price_high: Price::parse("92233720368.54").unwrap(),
                max_position: Quantity::parse("1").unwrap(),
                core_ratio: Rate::ZERO,
                batch_count: None,
                grid_spacing: Price::parse("1").unwrap(),
                grid_spread: Price::parse("1").unwrap(),
                throttle_interval: None,
            },
            ..long_grid_config()
        };

        assert_eq!(
            raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::PriceOutOfRange {
                field: "price high",
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
        let raw = StrategyConfig {
            long: SideConfig {
                price_low: Price::parse("74.99999994").unwrap(),
                price_high: Price::parse("200").unwrap(),
                max_position: Quantity::parse("1000").unwrap(),
                core_ratio: Rate::parse("0.7").unwrap(),
                batch_count: Some(5),
                grid_spacing: Price::parse("1").unwrap(),
                grid_spread: Price::parse("1").unwrap(),
                throttle_interval: None,
            },
            ..long_grid_config()
        };

        let normalized = raw.normalize(&spec, &btc_fees()).unwrap();

        assert_eq!(
            normalized.batches.batch(BatchIndex(1)).unwrap().price,
            Price::parse("174.99999998").unwrap()
        );
        let cell = normalized
            .grid
            .cells()
            .iter()
            .find(|cell| cell.buy_price == Price::parse("175").unwrap())
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
        let raw = StrategyConfig {
            long: SideConfig {
                price_low: Price::parse("92233720300").unwrap(),
                price_high: Price::parse("92233720301").unwrap(),
                max_position: Quantity::parse("1").unwrap(),
                grid_spacing: Price::parse("1").unwrap(),
                grid_spread: Price::parse("1").unwrap(),
                ..long_grid_config().long
            },
            ..long_grid_config()
        };

        assert_eq!(
            raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::GridSpreadUnreachable {
                buy_price: Price::parse("92233720301").unwrap(),
                taker_fee_rate: Rate::parse("0.00045").unwrap(),
            }
        );
    }

    #[test]
    fn rejects_a_missing_or_zero_batch_count_when_the_core_ratio_is_above_zero() {
        let mut missing = core_config();
        missing.long.batch_count = None;
        assert_eq!(
            missing.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::BatchCountOutOfRange { batch_count: None }
        );

        let mut zero = core_config();
        zero.long.batch_count = Some(0);
        assert_eq!(
            zero.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::BatchCountOutOfRange {
                batch_count: Some(0)
            }
        );
    }

    #[test]
    fn rejects_more_batches_than_the_supported_maximum_before_allocating() {
        let mut raw = core_config();
        raw.long.batch_count = Some(u32::MAX);

        assert_eq!(
            raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::TooManyBatches {
                batch_count: u32::MAX,
                max_batch_count: MAX_CELL_COUNT,
            }
        );
    }

    #[test]
    fn ignores_the_batch_count_when_the_core_ratio_is_zero() {
        let mut raw = long_grid_config();
        raw.long.batch_count = Some(3);

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert!(normalized.batches.is_empty());
        assert_eq!(normalized.config.long.batch_count, None);
        assert_eq!(normalized.grid.cells().len(), 20);
    }

    #[test]
    fn rejects_a_batch_whose_core_buy_is_below_the_minimum_order_amount() {
        // 每批 0.001，主仓部分是 0.0001，95000 的批次价下只有 9.5 美元。
        let raw = StrategyConfig {
            long: SideConfig {
                core_ratio: Rate::parse("0.1").unwrap(),
                batch_count: Some(4),
                max_position: Quantity::parse("0.004").unwrap(),
                ..config("75000", "95000", "0.004").long
            },
            ..config("75000", "95000", "0.004")
        };

        let error = raw.normalize(&fine_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::BatchCoreBelowMinimumOrderNotional {
                batch_index: BatchIndex(0),
                batch_price: Price::parse("95000").unwrap(),
                core_quantity: Quantity::parse("0.0001").unwrap(),
                required_quantity: Quantity::parse("0.00011").unwrap(),
            }
        );
        assert!(error.to_string().contains("fewer batches"));

        // 同样的比例与批数下，总量大一些每批的主仓就够最低金额了。
        let mut larger = raw.clone();
        larger.long.max_position = Quantity::parse("0.05").unwrap();
        assert!(larger.normalize(&fine_spec(), &btc_fees()).is_ok());
    }

    #[test]
    fn b23_rejects_cell_below_minimum_order_notional_and_reports_max_cell_count() {
        let raw = config("90000", "95000", "0.005");

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::CellBelowMinimumOrderNotional {
                cell_index: CellIndex(4),
                cell_price: Price::parse("91000").unwrap(),
                cell_quantity: Quantity::parse("0.001").unwrap(),
                required_quantity: Quantity::parse("0.003").unwrap(),
                max_cell_count: 1,
            }
        );
        assert!(error.to_string().contains("at most 1 cells fit"));
    }

    #[test]
    fn b23_rejects_grid_spread_that_is_not_profitable_after_taker_fees() {
        let mut raw = long_grid_config();
        raw.long.grid_spread = Price::parse("1").unwrap();

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::GridSpreadNotProfitable {
                grid_spread: Price::parse("1").unwrap(),
                minimum_grid_spread: Price::parse("86").unwrap(),
                taker_fee_rate: Rate::parse("0.00045").unwrap(),
            }
        );
    }

    #[test]
    fn b23_accepts_a_grid_that_survives_taker_fees_on_both_legs() {
        let mut raw = long_grid_config();
        raw.long.grid_spread = Price::parse("86").unwrap();

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert_eq!(
            normalized.grid.cells()[0].sell_price,
            Price::parse("95086").unwrap()
        );
    }

    #[test]
    fn rejects_configs_that_break_the_grid_shape() {
        let spec = btc_spec();
        let fees = btc_fees();

        let mut inverted = long_grid_config();
        inverted.long.price_low = Price::parse("95000").unwrap();
        inverted.long.price_high = Price::parse("95000").unwrap();
        assert!(matches!(
            inverted.normalize(&spec, &fees).unwrap_err(),
            ConfigError::PriceRangeNotAscending { .. }
        ));

        // 规整到 0：低于半个报价单位，网格无法比一个报价单位更密。
        let mut dense = long_grid_config();
        dense.long.grid_spacing = Price::parse("0.4").unwrap();
        assert!(matches!(
            dense.normalize(&spec, &fees).unwrap_err(),
            ConfigError::GridSpacingBelowOneTick { .. }
        ));

        let mut narrow = long_grid_config();
        narrow.long.grid_spread = Price::parse("0.4").unwrap();
        assert!(matches!(
            narrow.normalize(&spec, &fees).unwrap_err(),
            ConfigError::GridSpreadBelowOneTick { .. }
        ));

        let mut tiny = long_grid_config();
        tiny.long.max_position = Quantity::parse("0.0009").unwrap();
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
        raw.long.price_low = Price::parse("1").unwrap();
        raw.long.price_high = Price::parse("95000").unwrap();
        raw.long.grid_spacing = Price::parse("1").unwrap();
        raw.long.max_position = Quantity::parse("100000").unwrap();

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::TooManyCells {
                cell_count: 94_999,
                max_cell_count: MAX_CELL_COUNT,
            }
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
        candidate.long.price_low = Price::parse("94000").unwrap();
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

    /// 限速间隔是配置的一部分：规整不改动它，写进数据库再读回来还是同一个值。
    #[test]
    fn b44_the_throttle_interval_survives_normalizing_and_json() {
        let mut raw = core_config();
        raw.long.throttle_interval = Some(Duration::minutes(60));

        let normalized = raw.normalize(&btc_spec(), &btc_fees()).unwrap();

        assert_eq!(
            normalized.config.long.throttle_interval,
            Some(Duration::minutes(60))
        );
        let json = serde_json::to_string(&normalized.config).unwrap();
        let restored: StrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, normalized.config);
        assert_eq!(restored.long.throttle_interval, Some(Duration::minutes(60)));

        // 不限速也是配置的一部分：`None` 不会在读回来之后变成别的值。
        let plain = long_grid_config()
            .normalize(&btc_spec(), &btc_fees())
            .unwrap();
        let restored: StrategyConfig =
            serde_json::from_str(&serde_json::to_string(&plain.config).unwrap()).unwrap();
        assert_eq!(restored.long.throttle_interval, None);
    }

    #[test]
    fn rejects_a_negative_throttle_interval() {
        let mut raw = core_config();
        raw.long.throttle_interval = Some(Duration::seconds(-1));

        assert_eq!(
            raw.normalize(&btc_spec(), &btc_fees()).unwrap_err(),
            ConfigError::NegativeThrottleInterval {
                interval: Duration::seconds(-1),
            }
        );

        // 零是允许的：相邻两个单位仍然要等前一个单位先成交。
        raw.long.throttle_interval = Some(Duration::zero());
        assert!(raw.normalize(&btc_spec(), &btc_fees()).is_ok());
    }
}
