//! 策略配置与配置校验。
//!
//! 校验都使用规整后的值：价格按报价精度规整，数量按数量精度向下规整。

use chrono::Duration;
use serde::{Deserialize, Serialize};

use crate::batch::{BatchPlan, build_batches};
use crate::error::ConfigError;
use crate::grid::{
    Cell, GridPlan, GridShape, build_cells, cell_count, cell_prices, max_cell_count,
    meets_minimum_cell_notional, minimum_grid_spread, required_cell_quantity, spread_is_profitable,
};
use crate::instrument::{Fees, Instrument, InstrumentSpec};
use crate::units::{Price, PriceRounding, Quantity, Rate, SCALE};

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

impl StrategyConfig {
    /// 规整配置、划分批次与格子并校验。
    pub(crate) fn normalize(
        &self,
        spec: &InstrumentSpec,
        fees: &Fees,
    ) -> Result<NormalizedConfig, ConfigError> {
        self.check_instrument(spec)?;
        // 符号检查放在规整之前：规整只对非负值有意义，负价格、负间距、负数量
        // 应该在这里变成配置错误，而不是走到规整里的断言上。
        self.check_signs()?;

        let side = self.normalize_side(spec)?;
        let prices = side.cell_buy_prices()?;
        let (batches, cells) = side.build_plan(&prices, spec)?;
        check_cell_notional(&cells, &side, spec)?;
        check_cell_profitability(&cells, fees, side.grid_spread, side.price_tick)?;

        Ok(NormalizedConfig {
            config: side.to_config(&self.instrument, self.long.throttle_interval),
            batches,
            grid: GridPlan::from_cells(cells),
        })
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

    /// 规整一侧的价格与数量，并把比例与批数收拢成后续步骤要用的值。
    ///
    /// 检查顺序固定：价格范围 → 数量 → 比例与批数。这是原来那个大函数里的顺序，
    /// 同时有多处不合法时报出来的仍然是第一个。
    fn normalize_side(&self, spec: &InstrumentSpec) -> Result<NormalizedSide, ConfigError> {
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

        Ok(NormalizedSide {
            price_low,
            price_high,
            price_tick,
            grid_spacing,
            grid_spread,
            max_position,
            core_ratio,
            batch_count,
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

/// 规整后的一侧参数：价格已按报价精度规整，数量已按数量精度向下规整。
///
/// 批次划分、格子划分和最终配置都由它构造，所以这些值只算一次。
struct NormalizedSide {
    price_low: Price,
    price_high: Price,
    price_tick: Price,
    grid_spacing: Price,
    grid_spread: Price,
    max_position: Quantity,
    core_ratio: Rate,
    /// 主仓批数；主仓比例为 0 时没有批次，这里是 `None`。
    batch_count: Option<u32>,
}

impl NormalizedSide {
    /// 格子的买价，从高到低。
    ///
    /// 主仓比例为 1 时只积累主仓、没有网格（第 2.3 节）：不生成格子，网格间距、价差
    /// 和格子金额都不需要，也就不校验。否则先要求间距与价差至少一个报价精度，再按范围
    /// 与间距算出格数——先算格数再构造价格，极端配置不能因为分配内存而拖死进程。
    fn cell_buy_prices(&self) -> Result<Vec<Price>, ConfigError> {
        if self.core_ratio.units() == SCALE {
            return Ok(Vec::new());
        }
        if self.grid_spacing.units() < self.price_tick.units() {
            return Err(ConfigError::GridSpacingBelowOneTick {
                grid_spacing: self.grid_spacing,
                price_tick: self.price_tick,
            });
        }
        if self.grid_spread.units() < self.price_tick.units() {
            return Err(ConfigError::GridSpreadBelowOneTick {
                grid_spread: self.grid_spread,
                price_tick: self.price_tick,
            });
        }
        let count = cell_count(self.price_high, self.price_low, self.grid_spacing);
        if count > MAX_CELL_COUNT as i64 {
            return Err(ConfigError::TooManyCells {
                cell_count: count as usize,
                max_cell_count: MAX_CELL_COUNT,
            });
        }
        Ok(cell_prices(
            self.price_high,
            self.price_low,
            self.grid_spacing,
        ))
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
                batch_count,
                max_batch_count: MAX_CELL_COUNT,
            });
        }
        let shape = self.grid_shape();
        match self.batch_count {
            Some(batch_count) => build_batches(prices, &shape, batch_count, spec),
            None => Ok((BatchPlan::default(), build_cells(prices, &shape, spec)?)),
        }
    }

    fn grid_shape(&self) -> GridShape {
        GridShape {
            price_low: self.price_low,
            price_high: self.price_high,
            max_position: self.max_position,
            core_ratio: self.core_ratio,
            grid_spread: self.grid_spread,
        }
    }

    /// 规整后的配置：只有限速间隔不在规整范围内，由调用方传进来。
    fn to_config(
        &self,
        instrument: &Instrument,
        throttle_interval: Option<Duration>,
    ) -> StrategyConfig {
        StrategyConfig {
            instrument: instrument.clone(),
            long: SideConfig {
                price_low: self.price_low,
                price_high: self.price_high,
                max_position: self.max_position,
                core_ratio: self.core_ratio,
                batch_count: self.batch_count,
                grid_spacing: self.grid_spacing,
                grid_spread: self.grid_spread,
                throttle_interval,
            },
        }
    }
}

/// 每一格的金额都要够最低下单金额。
///
/// 按两条腿中较低的价格计算：多头侧的最低腿是格子买价，价格最低的一格最严格。
/// 这里逐格检查，不依赖这条推理；没有网格时 `cells` 是空的，检查自然跳过。
fn check_cell_notional(
    cells: &[Cell],
    side: &NormalizedSide,
    spec: &InstrumentSpec,
) -> Result<(), ConfigError> {
    for cell in cells.iter().rev() {
        if !meets_minimum_cell_notional(cell.quantity, cell.buy_price, spec) {
            return Err(ConfigError::CellBelowMinimumOrderNotional {
                cell_index: cell.index,
                cell_price: cell.buy_price,
                cell_quantity: cell.quantity,
                required_quantity: required_cell_quantity(cell.buy_price, spec),
                max_cell_count: max_cell_count(
                    cells.len(),
                    side.price_high,
                    side.max_position,
                    spec,
                    side.grid_spacing,
                ),
            });
        }
    }
    Ok(())
}

/// 每一格的价差要覆盖两条腿的 taker 费率（第 2.3 节）。价格最高的一格最严格。
fn check_cell_profitability(
    cells: &[Cell],
    fees: &Fees,
    grid_spread: Price,
    price_tick: Price,
) -> Result<(), ConfigError> {
    for cell in cells {
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
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{ConfigError, MAX_CELL_COUNT, SideConfig, StrategyConfig};
    use crate::batch::BatchIndex;
    use crate::fixtures::{btc_fees, btc_spec, core_only_config, fine_spec, long_grid_config};
    use crate::grid::{CellIndex, required_cell_quantity};
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
