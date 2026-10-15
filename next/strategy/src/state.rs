//! 策略状态：每批的主仓累计成交与结束标记，每个格子的启用标记与待买回量，
//! 以及限速的放行进度。
//!
//! 只有这几项需要保存，其余数量都可以推算：
//! 格子持有 = 格子数量 − 待买回量；主仓 = 各批累计成交之和；
//! 应有持仓 = 主仓 + 各已启用格子的持有量。

use serde::{Deserialize, Serialize};

use crate::batch::BatchIndex;
use crate::grid::{CellIndex, GridPlan};
use crate::throttle::ThrottleState;
use crate::units::{Position, Quantity};

/// 一个格子的状态。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellState {
    /// 是否已启用；已启用的格子不会关闭。
    pub enabled: bool,
    /// 待买回量 `S`，`0 ≤ S ≤ 格子数量`。
    pub to_buy_back: Quantity,
}

impl CellState {
    /// 持有量：已启用的格子中，已经买回、还没卖出的部分。
    pub fn held(&self, quantity: Quantity) -> Quantity {
        if self.enabled {
            quantity.saturating_sub(self.to_buy_back)
        } else {
            Quantity::ZERO
        }
    }
}

/// 一批主仓的状态。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BatchState {
    /// 主仓累计成交 `F`，`0 ≤ F ≤ C`。
    pub filled: Quantity,
    /// 是否已结束：主仓买完，或剩余量按批次价不足最低下单金额而提前结束。
    pub finished: bool,
}

/// 一个策略的状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyState {
    batches: Vec<BatchState>,
    cells: Vec<CellState>,
    /// 本侧限速的放行进度（第 4.1 节）。做多网格只有一侧，所以只有一份。
    throttle: ThrottleState,
}

impl StrategyState {
    /// 不带限速进度的状态：限速进度从零开始，全部单位还没放行。
    ///
    /// 调用方要么用 [`crate::Strategy::initial_state`]，要么自己给出限速进度
    /// （[`StrategyState::set_throttle`]）。
    pub fn new(batches: Vec<BatchState>, cells: Vec<CellState>) -> Self {
        Self {
            batches,
            cells,
            throttle: ThrottleState::default(),
        }
    }

    pub fn throttle(&self) -> &ThrottleState {
        &self.throttle
    }

    /// 可变的限速进度：放行和记录成交时刻都要改它。
    pub fn throttle_mut(&mut self) -> &mut ThrottleState {
        &mut self.throttle
    }

    pub fn set_throttle(&mut self, throttle: ThrottleState) {
        self.throttle = throttle;
    }

    pub fn batches(&self) -> &[BatchState] {
        &self.batches
    }

    pub fn batch(&self, index: BatchIndex) -> Option<&BatchState> {
        self.batches.get(index.0)
    }

    pub fn batch_mut(&mut self, index: BatchIndex) -> Option<&mut BatchState> {
        self.batches.get_mut(index.0)
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

    /// 主仓合计：各批累计成交之和。
    pub fn core_position(&self) -> Quantity {
        self.batches
            .iter()
            .fold(Quantity::ZERO, |total, batch| total + batch.filled)
    }

    /// 格子持有合计：各已启用格子的持有量之和。
    pub fn grid_position(&self, grid: &GridPlan) -> Quantity {
        grid.cells()
            .iter()
            .filter_map(|cell| Some(self.cell(cell.index)?.held(cell.quantity)))
            .fold(Quantity::ZERO, |total, held| total + held)
    }

    /// 应有持仓：主仓合计 + 格子持有。做多时为正。
    pub fn expected_position(&self, grid: &GridPlan) -> Position {
        Position::from_quantity(self.core_position() + self.grid_position(grid))
    }
}

/// 状态与批次、格子划分不一致。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("state has {actual} batches but the plan has {expected}")]
    BatchCountMismatch { expected: usize, actual: usize },
    #[error("state has {actual} cells but the grid has {expected}")]
    CellCountMismatch { expected: usize, actual: usize },
    #[error("cell {cell_index} to buy back {to_buy_back} exceeds its quantity {cell_quantity}")]
    ToBuyBackAboveCellQuantity {
        cell_index: CellIndex,
        to_buy_back: Quantity,
        cell_quantity: Quantity,
    },
    #[error("batch {batch_index} filled {filled}, above its core quantity {core_quantity}")]
    FilledAboveCoreQuantity {
        batch_index: BatchIndex,
        filled: Quantity,
        core_quantity: Quantity,
    },
    #[error("throttle released {released} units but the plan has {unit_count}")]
    ThrottleReleasedAboveUnitCount { released: usize, unit_count: usize },
}

#[cfg(test)]
mod tests {
    use super::{BatchState, StateError, StrategyState};
    use crate::batch::BatchIndex;
    use crate::fixtures::{core_long_strategy, long_grid_strategy};
    use crate::grid::CellIndex;
    use crate::units::{Position, Quantity};

    #[test]
    fn expected_position_is_the_core_plus_the_held_cells() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();

        // 主仓成交 .175，格子全部启用但还没买：持仓就是主仓。
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.175").unwrap();
        strategy.advance(&mut state, &[]);
        assert_eq!(state.core_position(), Quantity::parse("0.175").unwrap());
        assert_eq!(state.grid_position(strategy.grid()), Quantity::ZERO);
        assert_eq!(
            strategy.expected_position(&state),
            Position::parse("0.175").unwrap()
        );

        // 95、94 两格买回：各自 .015 计入持仓。
        for index in 0..2 {
            state.cell_mut(CellIndex(index)).unwrap().to_buy_back = Quantity::ZERO;
        }
        assert_eq!(
            state.grid_position(strategy.grid()),
            Quantity::parse("0.03").unwrap()
        );
        assert_eq!(
            strategy.expected_position(&state),
            Position::parse("0.205").unwrap()
        );

        // 95 格卖回：持仓减回主仓加剩下那一格。
        state.cell_mut(CellIndex(0)).unwrap().to_buy_back = Quantity::parse("0.015").unwrap();
        assert_eq!(
            strategy.expected_position(&state),
            Position::parse("0.19").unwrap()
        );
    }

    #[test]
    fn disabled_cells_hold_nothing_even_with_a_partial_to_buy_back() {
        let strategy = long_grid_strategy();
        let mut state = strategy.initial_state();
        state.cell_mut(CellIndex(0)).unwrap().enabled = false;

        assert_eq!(state.grid_position(strategy.grid()), Quantity::ZERO);
        assert_eq!(strategy.expected_position(&state), Position::ZERO);
    }

    #[test]
    fn validate_state_rejects_state_that_does_not_match_the_plan() {
        let strategy = core_long_strategy();
        let state = strategy.initial_state();
        assert!(strategy.validate_state(&state).is_ok());

        let fewer_batches = StrategyState::new(vec![BatchState::default()], state.cells().to_vec());
        assert!(matches!(
            strategy.validate_state(&fewer_batches).unwrap_err(),
            StateError::BatchCountMismatch {
                expected: 4,
                actual: 1
            }
        ));

        let fewer_cells = StrategyState::new(state.batches().to_vec(), vec![]);
        assert!(matches!(
            strategy.validate_state(&fewer_cells).unwrap_err(),
            StateError::CellCountMismatch {
                expected: 20,
                actual: 0
            }
        ));

        // 主仓成交超过 C = .175。
        let mut overfilled = state.clone();
        overfilled.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.2").unwrap();
        assert!(matches!(
            strategy.validate_state(&overfilled).unwrap_err(),
            StateError::FilledAboveCoreQuantity { .. }
        ));

        // 待买回量超过格子数量 .015。
        let mut too_much_to_buy_back = state.clone();
        too_much_to_buy_back
            .cell_mut(CellIndex(0))
            .unwrap()
            .to_buy_back = Quantity::parse("0.02").unwrap();
        assert!(matches!(
            strategy.validate_state(&too_much_to_buy_back).unwrap_err(),
            StateError::ToBuyBackAboveCellQuantity { .. }
        ));
    }
}
