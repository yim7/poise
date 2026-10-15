//! 预览数据：批次表与启动后会立即成交的订单（第 2.4、4.6 节）。
//!
//! 纯计算，供网页预览使用。启动后会立即成交的订单按启动时的第一批订单推算：
//! 主仓买单挂在批次价上，批次价已经不低于当前价时它立即成交；这一批的格子因此
//! 全部启用，格子买价也不低于当前价的那些格子随即买入。开启限速时启动只放行
//! 第一个单位，其余单位不会挂单，也就不会立即成交（第 4.1 节）。

use serde::{Deserialize, Serialize};

use crate::Strategy;
use crate::batch::BatchIndex;
use crate::order::{OrderPurpose, Unit};
use crate::state::StrategyState;
use crate::units::{Price, Quantity};

/// 批次表的一行：预览和运行中查看都用它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchSummary {
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
    /// 批次表。`state` 为空状态时就是启动时的样子。
    pub fn batch_summaries(&self, state: &StrategyState) -> Vec<BatchSummary> {
        self.batches()
            .batches()
            .iter()
            .map(|batch| {
                let batch_state = state.batch(batch.index).cloned().unwrap_or_default();
                let enabled_cells: Vec<_> = batch
                    .cells
                    .iter()
                    .filter_map(|cell_index| self.grid().cell(*cell_index))
                    .filter(|cell| {
                        state
                            .cell(cell.index)
                            .map(|cell_state| cell_state.enabled)
                            .unwrap_or(false)
                    })
                    .collect();
                let enabled_grid = enabled_cells
                    .iter()
                    .fold(Quantity::ZERO, |total, cell| total + cell.quantity);
                let finished = batch_state.finished;
                BatchSummary {
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

    /// 启动预览：批次表与启动后会立即成交的订单。
    pub fn startup_preview(&self, market_price: Price) -> StartupPreview {
        let state = self.initial_state();
        StartupPreview {
            market_price,
            batches: self.batch_summaries(&state),
            immediate_fills: self.immediate_fills(market_price),
        }
    }

    /// 启动后会立即成交的订单。
    ///
    /// 假设主仓买单一旦成交就整批成交（限价单挂在批次价上，成交即全部成交），
    /// 因此这一批的格子会全部启用；格子买价低于当前价的不在列表里。
    ///
    /// 开启限速时启动只放行第一个单位（第 4.1 节），后面那些还没放行的单位这一步根本
    /// 不会挂单，所以也不会立即成交：预览不能把它们算进来。
    pub fn immediate_fills(&self, market_price: Price) -> Vec<ImmediateFill> {
        let state = self.initial_state();
        let mut fills = Vec::new();
        if self.batches().is_empty() {
            // 没有主仓：格子就是限速单位，只有已放行的格子会挂单。
            for cell in self.grid().cells() {
                if !self.is_released(&state, Unit::Cell(cell.index)) {
                    break;
                }
                if cell.buy_price >= market_price {
                    fills.push(ImmediateFill {
                        unit: Unit::Cell(cell.index),
                        purpose: OrderPurpose::GridBuy,
                        limit_price: cell.buy_price,
                        quantity: cell.quantity,
                    });
                }
            }
            return fills;
        }

        // 批次价从高到低：某一批的批次价已经低于当前价时，后面的批次更低，都不会成交。
        for batch in self.batches().batches() {
            if batch.price < market_price {
                break;
            }
            if !self.is_released(&state, Unit::Batch(batch.index)) {
                break;
            }
            fills.push(ImmediateFill {
                unit: Unit::Batch(batch.index),
                purpose: OrderPurpose::CoreBuy,
                limit_price: batch.price,
                quantity: batch.core_quantity,
            });
            for cell_index in &batch.cells {
                let Some(cell) = self.grid().cell(*cell_index) else {
                    continue;
                };
                if cell.buy_price >= market_price {
                    fills.push(ImmediateFill {
                        unit: Unit::Cell(cell.index),
                        purpose: OrderPurpose::GridBuy,
                        limit_price: cell.buy_price,
                        quantity: cell.quantity,
                    });
                }
            }
        }
        fills
    }
}

#[cfg(test)]
mod tests {
    use super::{ImmediateFill, StartupPreview};
    use crate::batch::BatchIndex;
    use crate::fixtures::{
        core_long_strategy, long_grid_strategy, single_small_batch_strategy,
        throttled_core_long_strategy, throttled_long_grid_strategy,
    };
    use crate::grid::CellIndex;
    use crate::order::{OrderPurpose, Unit};
    use crate::units::{Price, Quantity};

    fn fill(unit: Unit, limit_price: &str, quantity: &str) -> ImmediateFill {
        ImmediateFill {
            unit,
            purpose: OrderPurpose::GridBuy,
            limit_price: Price::parse(limit_price).unwrap(),
            quantity: Quantity::parse(quantity).unwrap(),
        }
    }

    fn core_fill(index: usize, limit_price: &str, quantity: &str) -> ImmediateFill {
        ImmediateFill {
            unit: Unit::Batch(BatchIndex(index)),
            purpose: OrderPurpose::CoreBuy,
            limit_price: Price::parse(limit_price).unwrap(),
            quantity: Quantity::parse(quantity).unwrap(),
        }
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
            fill(Unit::Cell(CellIndex(0)), "95000", "0.015"),
            fill(Unit::Cell(CellIndex(1)), "94000", "0.015"),
            fill(Unit::Cell(CellIndex(2)), "93000", "0.015"),
            fill(Unit::Cell(CellIndex(3)), "92000", "0.015"),
            fill(Unit::Cell(CellIndex(4)), "91000", "0.015"),
            core_fill(1, "90000", "0.175"),
            fill(Unit::Cell(CellIndex(5)), "90000", "0.015"),
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
            &fill(Unit::Cell(CellIndex(19)), "76000", "0.015")
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
                fill(Unit::Cell(CellIndex(0)), "95000", "0.015"),
                fill(Unit::Cell(CellIndex(1)), "94000", "0.015"),
                fill(Unit::Cell(CellIndex(2)), "93000", "0.015"),
                fill(Unit::Cell(CellIndex(3)), "92000", "0.015"),
                fill(Unit::Cell(CellIndex(4)), "91000", "0.015"),
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
            vec![fill(Unit::Cell(CellIndex(0)), "95000", "0.05")]
        );
    }

    #[test]
    fn a_grid_without_core_lists_the_cells_that_fill_immediately() {
        let strategy = long_grid_strategy();

        let preview = strategy.startup_preview(Price::parse("92000").unwrap());

        assert!(preview.batches.is_empty());
        let expected = vec![
            fill(Unit::Cell(CellIndex(0)), "95000", "0.05"),
            fill(Unit::Cell(CellIndex(1)), "94000", "0.05"),
            fill(Unit::Cell(CellIndex(2)), "93000", "0.05"),
            fill(Unit::Cell(CellIndex(3)), "92000", "0.05"),
        ];
        assert_eq!(preview.immediate_fills, expected);
    }

    #[test]
    fn the_batch_table_shows_the_shortfall_of_a_batch_that_ended_early() {
        let strategy = single_small_batch_strategy();
        let mut state = strategy.initial_state();
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.02").unwrap();
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
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.07").unwrap();
        strategy.advance(&mut state, &[]);

        let batches = strategy.batch_summaries(&state);

        assert_eq!(batches[0].core_filled, Quantity::parse("0.07").unwrap());
        assert_eq!(batches[0].enabled_cell_count, 2);
        assert_eq!(batches[0].cell_count, 5);
        assert_eq!(batches[1].enabled_cell_count, 0);
    }
}
