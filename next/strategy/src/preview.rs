//! 预览数据：批次表与启动后会立即成交的订单（第 2.4、4.6 节）。
//!
//! 纯计算，供网页预览使用。两侧各按自己的启动顺序推算：主仓建仓单挂在批次价上，
//! 批次价已经到达当前价时它立即成交；这一批的格子因此全部启用，格子建仓价也已经
//! 到达当前价的那些格子随即建仓。开启限速时启动只放行第一个单位，其余单位不会挂单，
//! 也就不会立即成交（第 4.1 节）。

use serde::{Deserialize, Serialize};

use crate::Strategy;
use crate::batch::BatchIndex;
use crate::grid::Cell;
use crate::order::{OrderPurpose, Unit};
use crate::side::{Direction, SidePlan};
use crate::state::StrategyState;
use crate::units::{Price, Quantity};

/// 批次表的一行：预览和运行中查看都用它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchSummary {
    /// 属于哪一侧；中性网格两侧都没有主仓，只有单边网格才有行。
    pub direction: Direction,
    pub index: BatchIndex,
    /// 批次价。
    pub price: Price,
    /// 本批总量 `q`。
    pub quantity: Quantity,
    /// 主仓部分 `C`。
    pub core_quantity: Quantity,
    /// 网格部分 `G`。
    pub grid_quantity: Quantity,
    /// 主仓累计成交 `F`。
    pub core_filled: Quantity,
    /// 本批是否已结束。
    pub finished: bool,
    /// 跟随本批的格子数。
    pub cell_count: usize,
    /// 其中已启用的格子数。
    pub enabled_cell_count: usize,
    /// 提前结束时少建的主仓数量；正常结束时为 0。
    pub core_shortfall: Quantity,
    /// 提前结束时少建的网格数量；正常结束时为 0。
    pub grid_shortfall: Quantity,
}

/// 启动后会立即成交的一张订单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImmediateFill {
    /// 归属单位；它已经带上这一侧的方向。
    pub unit: Unit,
    pub purpose: OrderPurpose,
    pub limit_price: Price,
    pub quantity: Quantity,
}

/// 启动前的预览：批次表与启动后会立即成交的订单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupPreview {
    pub market_price: Price,
    pub batches: Vec<BatchSummary>,
    pub immediate_fills: Vec<ImmediateFill>,
}

impl Strategy {
    /// 两侧的批次表，按 [`Direction::ALL`] 的顺序。`state` 为空状态时就是启动时的样子。
    pub fn batch_summaries(&self, state: &StrategyState) -> Vec<BatchSummary> {
        self.sides()
            .iter()
            .flat_map(|side| batch_summaries_of(side, state))
            .collect()
    }

    /// 启动预览：批次表与启动后会立即成交的订单。
    pub fn startup_preview(&self, market_price: Price) -> StartupPreview {
        let state = self.initial_state();
        StartupPreview {
            market_price,
            batches: self.batch_summaries(&state),
            immediate_fills: self.immediate_fills(market_price),
        }
    }

    /// 启动后会立即成交的订单，两侧按 [`Direction::ALL`] 的顺序接在一起。
    pub fn immediate_fills(&self, market_price: Price) -> Vec<ImmediateFill> {
        let state = self.initial_state();
        self.sides()
            .iter()
            .flat_map(|side| self.side_immediate_fills(side, &state, market_price))
            .collect()
    }

    /// 一侧启动后会立即成交的订单。
    ///
    /// 假设主仓建仓单一旦成交就整批成交（限价单挂在批次价上，成交即全部成交），
    /// 因此这一批的格子会全部启用；格子建仓价还没到当前价的那些格子不建仓。
    ///
    /// 开启限速时启动只放行第一个单位（第 4.1 节），后面那些还没放行的单位这一步根本
    /// 不会挂单，所以也不会立即成交：预览不能把它们算进来。
    fn side_immediate_fills(
        &self,
        side: &SidePlan,
        state: &StrategyState,
        market_price: Price,
    ) -> Vec<ImmediateFill> {
        let direction = side.direction();
        let mut fills = Vec::new();
        if side.batches().is_empty() {
            // 没有主仓：格子就是限速单位，只有已放行的格子会挂单。
            for cell in side.grid().cells() {
                let unit = Unit::Cell(direction, cell.index);
                if !self.is_released(state, unit) {
                    break;
                }
                if let Some(fill) = cell_entry_fill(direction, cell, market_price) {
                    fills.push(fill);
                }
            }
            return fills;
        }

        // 批次价按启动顺序排列：某一批已经到不了当前价时，后面的批次更到不了，都不会成交。
        for batch in side.batches().batches() {
            let unit = Unit::Batch(direction, batch.index);
            if !direction.reached(batch.price, market_price) {
                break;
            }
            if !self.is_released(state, unit) {
                break;
            }
            fills.push(ImmediateFill {
                unit,
                purpose: OrderPurpose::Core,
                limit_price: batch.price,
                quantity: batch.core_quantity,
            });
            for cell_index in &batch.cells {
                let Some(cell) = side.grid().cell(*cell_index) else {
                    continue;
                };
                if let Some(fill) = cell_entry_fill(direction, cell, market_price) {
                    fills.push(fill);
                }
            }
        }
        fills
    }
}

/// 一侧的批次表。
fn batch_summaries_of(side: &SidePlan, state: &StrategyState) -> Vec<BatchSummary> {
    let side_state = state.side(side.direction());
    side.batches()
        .batches()
        .iter()
        .map(|batch| {
            let batch_state = side_state
                .and_then(|state| state.batch(batch.index))
                .cloned()
                .unwrap_or_default();
            let enabled_cells: Vec<&Cell> = batch
                .cells
                .iter()
                .filter_map(|index| side.grid().cell(*index))
                .filter(|cell| {
                    side_state
                        .and_then(|state| state.cell(cell.index))
                        .is_some_and(|cell_state| cell_state.enabled)
                })
                .collect();
            let enabled_grid = enabled_cells
                .iter()
                .fold(Quantity::ZERO, |total, cell| total + cell.quantity);
            let finished = batch_state.finished;
            BatchSummary {
                direction: side.direction(),
                index: batch.index,
                price: batch.price,
                quantity: batch.quantity,
                core_quantity: batch.core_quantity,
                grid_quantity: batch.grid_quantity,
                core_filled: batch_state.filled,
                finished,
                cell_count: batch.cells.len(),
                enabled_cell_count: enabled_cells.len(),
                core_shortfall: if finished {
                    batch.core_quantity.saturating_sub(batch_state.filled)
                } else {
                    Quantity::ZERO
                },
                grid_shortfall: if finished {
                    batch.grid_quantity.saturating_sub(enabled_grid)
                } else {
                    Quantity::ZERO
                },
            }
        })
        .collect()
}

/// 一格在启动时会不会立即建仓：它的建仓价已经到达当前价。
fn cell_entry_fill(
    direction: Direction,
    cell: &Cell,
    market_price: Price,
) -> Option<ImmediateFill> {
    direction
        .reached(cell.entry_price, market_price)
        .then_some(ImmediateFill {
            unit: Unit::Cell(direction, cell.index),
            purpose: OrderPurpose::GridEntry,
            limit_price: cell.entry_price,
            quantity: cell.quantity,
        })
}

#[cfg(test)]
mod tests {
    use super::{ImmediateFill, StartupPreview};
    use crate::Strategy;
    use crate::batch::BatchIndex;
    use crate::config::SideConfig;
    use crate::fixtures::{
        btc_fees, cheap_spec, core_long_strategy, core_short_strategy, long_grid_strategy,
        neutral_grid_config, neutral_grid_strategy, single_small_batch_strategy,
        throttled_core_long_strategy, throttled_long_grid_strategy,
    };
    use crate::grid::CellIndex;
    use crate::order::{OrderPurpose, Unit};
    use crate::side::Direction;
    use crate::state::StrategyState;
    use crate::units::{Price, Quantity};
    use chrono::Duration;

    fn fill(unit: Unit, limit_price: &str, quantity: &str) -> ImmediateFill {
        ImmediateFill {
            unit,
            purpose: OrderPurpose::GridEntry,
            limit_price: Price::parse(limit_price).unwrap(),
            quantity: Quantity::parse(quantity).unwrap(),
        }
    }

    fn core_fill(index: usize, limit_price: &str, quantity: &str) -> ImmediateFill {
        ImmediateFill {
            unit: Unit::Batch(Direction::Long, BatchIndex(index)),
            purpose: OrderPurpose::Core,
            limit_price: Price::parse(limit_price).unwrap(),
            quantity: Quantity::parse(quantity).unwrap(),
        }
    }

    fn long_cell(index: usize) -> Unit {
        Unit::Cell(Direction::Long, CellIndex(index))
    }

    fn short_cell(index: usize) -> Unit {
        Unit::Cell(Direction::Short, CellIndex(index))
    }

    fn long_state_mut(state: &mut StrategyState) -> &mut crate::state::SideState {
        state
            .side_mut(Direction::Long)
            .expect("the long side is configured")
    }

    #[test]
    fn the_preview_shows_the_batch_table_before_starting() {
        let strategy = core_long_strategy();

        let StartupPreview {
            market_price,
            batches,
            ..
        } = strategy.startup_preview(Price::parse("96000").unwrap());

        assert_eq!(market_price, Price::parse("96000").unwrap());
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
            assert_eq!(batch.cell_count, 5);
            assert_eq!(batch.enabled_cell_count, 0);
            assert_eq!(batch.core_filled, Quantity::ZERO);
            assert!(!batch.finished);
            assert_eq!(batch.core_shortfall, Quantity::ZERO);
            assert_eq!(batch.grid_shortfall, Quantity::ZERO);
        }
    }

    #[test]
    fn the_preview_lists_the_orders_that_fill_immediately() {
        let strategy = core_long_strategy();

        // 价格 90000：95 和 90 两批的主仓立即买入；这两批的格子买价不低于
        // 当前价的也立即买入（95 到 91，以及 90）。
        let preview = strategy.startup_preview(Price::parse("90000").unwrap());

        let expected = vec![
            core_fill(0, "95000", "0.175"),
            fill(long_cell(0), "95000", "0.015"),
            fill(long_cell(1), "94000", "0.015"),
            fill(long_cell(2), "93000", "0.015"),
            fill(long_cell(3), "92000", "0.015"),
            fill(long_cell(4), "91000", "0.015"),
            core_fill(1, "90000", "0.175"),
            fill(long_cell(5), "90000", "0.015"),
        ];
        assert_eq!(preview.immediate_fills, expected);
    }

    #[test]
    fn nothing_fills_immediately_when_the_price_is_above_the_range() {
        let strategy = core_long_strategy();

        let preview = strategy.startup_preview(Price::parse("96000").unwrap());

        assert!(preview.immediate_fills.is_empty());
    }

    #[test]
    fn every_batch_fills_immediately_when_the_price_is_at_the_bottom() {
        let strategy = core_long_strategy();

        let preview = strategy.startup_preview(Price::parse("75000").unwrap());

        // 四批主仓加全部 20 格。
        assert_eq!(preview.immediate_fills.len(), 24);
        assert_eq!(
            preview.immediate_fills.last().unwrap(),
            &fill(long_cell(19), "76000", "0.015")
        );
    }

    #[test]
    fn a_throttled_start_lists_only_the_units_it_releases() {
        let strategy = throttled_core_long_strategy();

        // 价格在全部批次价之下：没有限速时四批主仓会一起成交。
        assert_eq!(
            core_long_strategy()
                .immediate_fills(Price::parse("75000").unwrap())
                .len(),
            24
        );

        // 开启限速：启动只放行 95 批，其余三批不会挂单，也就不会立即成交。
        let preview = strategy.startup_preview(Price::parse("75000").unwrap());
        assert_eq!(
            preview.immediate_fills,
            vec![
                core_fill(0, "95000", "0.175"),
                fill(long_cell(0), "95000", "0.015"),
                fill(long_cell(1), "94000", "0.015"),
                fill(long_cell(2), "93000", "0.015"),
                fill(long_cell(3), "92000", "0.015"),
                fill(long_cell(4), "91000", "0.015"),
            ]
        );
    }

    #[test]
    fn a_throttled_grid_without_core_starts_with_the_first_cell_only() {
        let strategy = throttled_long_grid_strategy();

        let preview = strategy.startup_preview(Price::parse("92000").unwrap());

        // `r = 0`：格子就是单位，启动只放行最高的那一格。
        assert_eq!(
            preview.immediate_fills,
            vec![fill(long_cell(0), "95000", "0.05")]
        );
    }

    #[test]
    fn a_grid_without_core_lists_the_cells_that_fill_immediately() {
        let strategy = long_grid_strategy();

        let preview = strategy.startup_preview(Price::parse("92000").unwrap());

        assert!(preview.batches.is_empty());
        let expected = vec![
            fill(long_cell(0), "95000", "0.05"),
            fill(long_cell(1), "94000", "0.05"),
            fill(long_cell(2), "93000", "0.05"),
            fill(long_cell(3), "92000", "0.05"),
        ];
        assert_eq!(preview.immediate_fills, expected);
    }

    /// 空头侧是多头侧的镜像：价格越低越到不了，所以启动时立即成交的是低处的批次和格子。
    #[test]
    fn b10_the_preview_mirrors_the_long_side_on_the_short_side() {
        let strategy = core_short_strategy();

        // 价格 76000：75000 那一批的主仓立即开空，跟随它的 75、76 两格也立即开空；
        // 80 及以上的批次价格还没到。
        let preview = strategy.startup_preview(Price::parse("76000").unwrap());

        let expected = vec![
            ImmediateFill {
                unit: Unit::Batch(Direction::Short, BatchIndex(0)),
                purpose: OrderPurpose::Core,
                limit_price: Price::parse("75000").unwrap(),
                quantity: Quantity::parse("0.175").unwrap(),
            },
            fill(short_cell(0), "75000", "0.015"),
            fill(short_cell(1), "76000", "0.015"),
        ];
        assert_eq!(preview.immediate_fills, expected);
        // 批次表也是镜像：批次价从 `L` 往上，主仓与网格数量跟多头侧一样。
        assert_eq!(preview.batches.len(), 4);
        assert!(
            preview
                .batches
                .iter()
                .all(|batch| batch.direction == Direction::Short)
        );
        let prices: Vec<Price> = preview.batches.iter().map(|batch| batch.price).collect();
        assert_eq!(
            prices,
            ["75000", "80000", "85000", "90000"]
                .map(|price| Price::parse(price).unwrap())
                .to_vec()
        );

        // 价格在范围下方时一张也不会立即成交：空头侧的开空价都还没到。
        assert!(
            strategy
                .startup_preview(Price::parse("74000").unwrap())
                .immediate_fills
                .is_empty()
        );

        // 价格越过整个范围时反过来：四批主仓和二十格全部立即开空。
        let all = strategy.startup_preview(Price::parse("96000").unwrap());
        assert_eq!(all.immediate_fills.len(), 24);
        assert_eq!(
            all.immediate_fills.last().unwrap(),
            &fill(short_cell(19), "94000", "0.015")
        );
    }

    /// 中性网格（第 13 节 B12）：价格 93 已经越过空头侧的三格，多头侧一格都到不了。
    #[test]
    fn b12_the_preview_shows_the_short_cells_that_open_immediately() {
        let strategy = neutral_grid_strategy();

        let preview = strategy.startup_preview(Price::parse("93").unwrap());

        // 空头侧 91–96 五格各 .05，91、92、93 三格立即开空；多头侧最高买价 89，够不着。
        assert_eq!(
            preview.immediate_fills,
            vec![
                fill(short_cell(0), "91", "0.05"),
                fill(short_cell(1), "92", "0.05"),
                fill(short_cell(2), "93", "0.05"),
            ]
        );
        // 两侧都没有主仓，批次表是空的。
        assert!(preview.batches.is_empty());
    }

    /// 限速下启动只放行每一侧的第一个单位：空头侧的第一格是价格最低的那一格。
    #[test]
    fn b12_a_throttled_short_side_opens_only_its_first_cell() {
        let mut config = neutral_grid_config();
        config.short = Some(SideConfig {
            throttle_interval: Some(Duration::hours(1)),
            ..config.short.unwrap()
        });
        let strategy = Strategy::new(config, cheap_spec(), btc_fees()).unwrap();

        let preview = strategy.startup_preview(Price::parse("93").unwrap());

        // 空头侧只放行 91 那一格；92、93 还没放行，这一步根本不挂单。
        assert_eq!(
            preview.immediate_fills,
            vec![fill(short_cell(0), "91", "0.05")]
        );
        // 多头侧不限速，十格都放行，但价格 93 够不着它们的买价。
        assert!(preview.batches.is_empty());
    }

    #[test]
    fn the_batch_table_shows_the_shortfall_of_a_batch_that_ended_early() {
        let strategy = single_small_batch_strategy();
        let mut state = strategy.initial_state();
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.02").unwrap();
        strategy.advance(&mut state, &[]);

        let batches = strategy.batch_summaries(&state);

        let batch = &batches[0];
        assert!(batch.finished);
        assert_eq!(batch.core_filled, Quantity::parse("0.02").unwrap());
        // 少建的主仓 .09 和网格 .39。
        assert_eq!(batch.core_shortfall, Quantity::parse("0.09").unwrap());
        assert_eq!(batch.grid_shortfall, Quantity::parse("0.39").unwrap());
        assert_eq!(batch.enabled_cell_count, 0);
    }

    #[test]
    fn the_batch_table_counts_the_cells_enabled_so_far() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.07").unwrap();
        strategy.advance(&mut state, &[]);

        let batches = strategy.batch_summaries(&state);

        assert_eq!(batches[0].core_filled, Quantity::parse("0.07").unwrap());
        assert_eq!(batches[0].enabled_cell_count, 2);
        assert_eq!(batches[0].cell_count, 5);
        assert_eq!(batches[1].enabled_cell_count, 0);
    }
}
