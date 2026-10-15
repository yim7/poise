//! 策略配置与配置校验。
//!
//! 校验都使用规整后的值：价格按报价精度规整，数量按数量精度向下规整。

use serde::{Deserialize, Serialize};

use crate::grid::{Cell, CellIndex, GridPlan, cell_count, cell_prices};
use crate::instrument::{Fees, Instrument, InstrumentSpec};
use crate::units::{Notional, Price, PriceRounding, Quantity, Rate, SCALE, div_ceil};

/// 网格格数的技术上限。
///
/// 设计没有规定这个上限。它只用来挡住明显不合理的配置（例如范围极大、间距极小），
/// 正常配置远达不到；按 Hyperliquid 每日 5 万次操作上限，格数超过这个量级本身也不可用。
pub const MAX_CELL_COUNT: usize = 10_000;

/// 多头侧的配置。
///
/// 主仓比例、主仓批数、限速、观点失效价属于后续任务，这里还没有对应的字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideConfig {
    /// 本侧建仓的价格范围下限 `L`。
    pub price_low: Price,
    /// 本侧建仓的价格范围上限 `H`。
    pub price_high: Price,
    /// 本侧最多持有的数量 `M`。
    pub max_position: Quantity,
    /// 网格间距 `d`。
    pub grid_spacing: Price,
    /// 网格价差 `g`：每格卖价 = 格子买价 + g。
    pub grid_spread: Price,
}

/// 做多网格配置：只有多头侧，没有主仓，不限速。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub instrument: Instrument,
    pub long: SideConfig,
}

/// 规整后的配置与格子划分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NormalizedConfig {
    pub config: StrategyConfig,
    pub grid: GridPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("config instrument `{config}` does not match the exchange instrument `{spec}`")]
    InstrumentMismatch { config: String, spec: String },
    #[error("price range must be ascending after rounding, got {low} and {high}")]
    PriceRangeNotAscending { low: Price, high: Price },
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
        "grid spread {grid_spread} does not stay profitable after taker fees {taker_fee_rate} on both legs; at least {minimum_grid_spread} is required"
    )]
    GridSpreadNotProfitable {
        grid_spread: Price,
        minimum_grid_spread: Price,
        taker_fee_rate: Rate,
    },
}

impl StrategyConfig {
    /// 规整配置、划分格子并校验。
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

        let price_low = spec.normalize_price(self.long.price_low, PriceRounding::Nearest);
        let price_high = spec.normalize_price(self.long.price_high, PriceRounding::Nearest);
        if price_low >= price_high {
            return Err(ConfigError::PriceRangeNotAscending {
                low: price_low,
                high: price_high,
            });
        }

        let price_tick = spec.price_tick();
        let grid_spacing = spec.normalize_price(self.long.grid_spacing, PriceRounding::Nearest);
        if grid_spacing.units() < price_tick.units() {
            return Err(ConfigError::GridSpacingBelowOneTick {
                grid_spacing,
                price_tick,
            });
        }
        let grid_spread = spec.normalize_price(self.long.grid_spread, PriceRounding::Nearest);
        if grid_spread.units() < price_tick.units() {
            return Err(ConfigError::GridSpreadBelowOneTick {
                grid_spread,
                price_tick,
            });
        }

        let max_position = spec.normalize_quantity(self.long.max_position);
        if max_position.units() < spec.quantity_step().units() {
            return Err(ConfigError::MaxPositionBelowOneStep {
                max_position,
                quantity_step: spec.quantity_step(),
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
        let prices = cell_prices(price_high, price_low, grid_spacing);

        let cells = build_cells(&prices, max_position, spec, grid_spread);

        // 最低金额：每一格按两条腿中较低的价格计算。多头侧的最低腿是格子买价，
        // 价格最低的一格最严格；这里逐格检查，不依赖这条推理。
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
                    minimum_grid_spread(cell.buy_price, fees.taker_rate, price_tick);
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
                    grid_spacing,
                    grid_spread,
                },
            },
            grid: GridPlan::from_cells(cells),
        })
    }
}

/// 把 `M` 按数量最小单位平均分给各格，余下的最小单位依次给靠近空隙的格子。
fn build_cells(
    prices: &[Price],
    max_position: Quantity,
    spec: &InstrumentSpec,
    grid_spread: Price,
) -> Vec<Cell> {
    let step = spec.quantity_step();
    prices
        .iter()
        .enumerate()
        .map(|(index, price)| Cell {
            index: CellIndex(index),
            buy_price: *price,
            sell_price: price.saturating_add(grid_spread),
            quantity: cell_quantity(index, prices.len(), max_position, step),
        })
        .collect()
}

fn cell_quantity_steps(
    index: usize,
    cell_count: usize,
    max_position: Quantity,
    step: Quantity,
) -> i64 {
    let total_steps = max_position.units() / step.units();
    let base = total_steps / cell_count as i64;
    let remainder = total_steps % cell_count as i64;
    base + i64::from((index as i64) < remainder)
}

fn cell_quantity(
    index: usize,
    cell_count: usize,
    max_position: Quantity,
    step: Quantity,
) -> Quantity {
    Quantity::from_units(cell_quantity_steps(index, cell_count, max_position, step) * step.units())
}

/// 格子数量低于最低订单金额时，这一格至少需要多少数量。
fn required_cell_quantity(price: Price, spec: &InstrumentSpec) -> Quantity {
    let step_units = spec.quantity_step().units() as i128;
    let minimum_raw = 3 * spec.min_order_notional().raw();
    let price_units = price.units() as i128;
    let units = div_ceil(minimum_raw, price_units) + 2 * step_units;
    let units = div_ceil(units, step_units) * step_units;
    Quantity::from_units(units as i64)
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
        let price =
            Price::from_units(price_high.units() - (candidate as i64 - 1) * grid_spacing.units());
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
fn minimum_grid_spread(price: Price, taker: Rate, tick: Price) -> Price {
    let price_units = price.units() as i128;
    let taker_units = taker.units() as i128;
    let denominator = SCALE as i128 - taker_units;
    let tick_units = tick.units();
    if denominator <= 0 || price_units <= 0 {
        return Price::from_units(i64::MAX / 2);
    }
    let numerator = 2 * price_units * taker_units;
    let units = numerator / denominator + 1;
    let mut units = div_ceil(units, tick_units as i128) * tick_units as i128;
    while !spread_is_profitable(
        price,
        Price::from_units((price_units + units) as i64),
        taker,
    ) {
        units += tick_units as i128;
    }
    Price::from_units(units as i64)
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, MAX_CELL_COUNT, SideConfig, StrategyConfig};
    use crate::fixtures::{btc_fees, btc_spec, long_grid_config};
    use crate::instrument::Instrument;
    use crate::units::{Notional, Price, Quantity};

    fn config(price_low: &str, price_high: &str, max_position: &str) -> StrategyConfig {
        StrategyConfig {
            instrument: Instrument::try_new("BTC").unwrap(),
            long: SideConfig {
                price_low: Price::parse(price_low).unwrap(),
                price_high: Price::parse(price_high).unwrap(),
                max_position: Quantity::parse(max_position).unwrap(),
                grid_spacing: Price::parse("1000").unwrap(),
                grid_spread: Price::parse("1000").unwrap(),
            },
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
    fn b23_rejects_cell_below_minimum_order_notional_and_reports_max_cell_count() {
        let raw = config("90000", "95000", "0.005");

        let error = raw.normalize(&btc_spec(), &btc_fees()).unwrap_err();

        assert_eq!(
            error,
            ConfigError::CellBelowMinimumOrderNotional {
                cell_index: crate::grid::CellIndex(4),
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
                taker_fee_rate: crate::units::Rate::parse("0.00045").unwrap(),
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

        let mut candidate = raw.clone();
        candidate.long.price_low = Price::parse("94000").unwrap();
        assert!(candidate.normalize(&spec, &btc_fees()).is_ok());
        assert_eq!(max_cell_count, 1);

        let required = super::required_cell_quantity(Price::parse("91000").unwrap(), &spec);
        assert!(
            Notional::of(required, Price::parse("91000").unwrap())
                >= Notional::parse("212").unwrap()
        );
        assert!(
            Notional::of(
                Quantity::from_units(required.units() - spec.quantity_step().units()),
                Price::parse("91000").unwrap()
            ) < Notional::parse("212").unwrap()
        );
    }
}
