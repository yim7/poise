//! 策略状态：每个格子的启用标记与待买回量。
//!
//! 只有这两项需要保存，其余数量都可以推算：
//! 格子持有 = 格子数量 − 待买回量；应有持仓 = 各已启用格子的持有量之和。

use serde::{Deserialize, Serialize};

use crate::grid::{CellIndex, GridPlan};
use crate::units::{Position, Quantity};

/// 一个格子的状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellState {
    /// 是否已启用；已启用的格子不会关闭。
    pub enabled: bool,
    /// 待买回量 `S`，`0 ≤ S ≤ 格子数量`。
    pub to_buy_back: Quantity,
}

impl CellState {
    /// 格子刚启用时全部是待买回。
    pub fn enabled_all_to_buy_back(quantity: Quantity) -> Self {
        Self {
            enabled: true,
            to_buy_back: quantity,
        }
    }
}

/// 一个策略的格子状态，按格子下标排列。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyState {
    cells: Vec<CellState>,
}

impl StrategyState {
    pub fn new(cells: Vec<CellState>) -> Self {
        Self { cells }
    }

    pub fn cells(&self) -> &[CellState] {
        &self.cells
    }

    pub fn cell(&self, index: CellIndex) -> Option<&CellState> {
        self.cells.get(index.0)
    }

    pub fn cell_mut(&mut self, index: CellIndex) -> Option<&mut CellState> {
        self.cells.get_mut(index.0)
    }

    /// 已启用格子的持有量之和，做多时为正仓位。
    pub fn expected_position(&self, grid: &GridPlan) -> Position {
        let held = grid
            .cells()
            .iter()
            .filter_map(|cell| {
                let state = self.cell(cell.index)?;
                state
                    .enabled
                    .then(|| cell.quantity.saturating_sub(state.to_buy_back))
            })
            .fold(Quantity::ZERO, |total, held| total + held);
        Position::from_quantity(held)
    }
}

/// 状态与格子划分不一致。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("state has {actual} cells but the grid has {expected}")]
    CellCountMismatch { expected: usize, actual: usize },
    #[error("cell {cell_index} to buy back {to_buy_back} exceeds its quantity {cell_quantity}")]
    ToBuyBackAboveCellQuantity {
        cell_index: CellIndex,
        to_buy_back: Quantity,
        cell_quantity: Quantity,
    },
}
