//! 策略状态：每一侧各批主仓的累计成交与结束标记、各格子的启用标记与待买回量，
//! 以及每一侧的限速放行进度。
//!
//! 只有这几项需要保存，其余数量都可以推算：
//! 格子持有 = 格子数量 − 待买回量；主仓 = 各批累计成交之和；
//! 应有持仓 = 多头侧（主仓 + 已启用格子的持有量）− 空头侧（主仓 + 已启用格子的持有量）。

use serde::{Deserialize, Serialize};

use crate::batch::BatchIndex;
use crate::grid::{CellIndex, GridPlan};
use crate::side::Direction;
use crate::throttle::ThrottleState;
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
    /// 持有量：已启用的格子中，已经建仓、还没平掉的部分。
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
    /// 是否已结束：主仓建完，或剩余量按批次价不足最低下单金额而提前结束。
    pub finished: bool,
}

/// 一侧的状态：批次、格子与限速进度。两侧各有一份，互不影响。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideState {
    direction: Direction,
    batches: Vec<BatchState>,
    cells: Vec<CellState>,
    throttle: ThrottleState,
}

impl SideState {
    /// 不带限速进度的状态：限速进度从零开始，本侧全部单位还没放行。
    ///
    /// 调用方要么用 [`crate::Strategy::initial_state`]，要么自己给出限速进度
    /// （[`SideState::set_throttle`]）。
    pub fn new(direction: Direction, batches: Vec<BatchState>, cells: Vec<CellState>) -> Self {
        Self {
            direction,
            batches,
            cells,
            throttle: ThrottleState::default(),
        }
    }

    pub fn direction(&self) -> Direction {
        self.direction
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

    /// 本侧的主仓合计：各批累计成交之和。不带符号。
    pub fn core_position(&self) -> Quantity {
        self.batches
            .iter()
            .fold(Quantity::ZERO, |total, batch| total + batch.filled)
    }

    /// 本侧的格子持有合计：各已启用格子的持有量之和。不带符号。
    pub fn grid_position(&self, grid: &GridPlan) -> Quantity {
        grid.cells()
            .iter()
            .filter_map(|cell| Some(self.cell(cell.index)?.held(cell.quantity)))
            .fold(Quantity::ZERO, |total, held| total + held)
    }

    /// 本侧的持仓量：主仓合计 + 格子持有。不带符号，方向由 [`SideState::direction`] 给出。
    pub fn position(&self, grid: &GridPlan) -> Quantity {
        self.core_position() + self.grid_position(grid)
    }
}

/// 一个策略的状态：最多两侧，按 [`Direction::ALL`] 的顺序排列。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyState {
    sides: Vec<SideState>,
}

impl StrategyState {
    /// 各侧状态；配置了哪一侧就给哪一侧，顺序与 [`Direction::ALL`] 一致。
    pub fn new(sides: Vec<SideState>) -> Self {
        Self { sides }
    }

    pub fn sides(&self) -> &[SideState] {
        &self.sides
    }

    pub fn side(&self, direction: Direction) -> Option<&SideState> {
        self.sides.iter().find(|side| side.direction() == direction)
    }

    pub fn side_mut(&mut self, direction: Direction) -> Option<&mut SideState> {
        self.sides
            .iter_mut()
            .find(|side| side.direction() == direction)
    }
}

/// 状态与批次、格子划分不一致。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("the plan has sides {plan:?} but the state has {state:?}")]
    SideMismatch {
        plan: Vec<Direction>,
        state: Vec<Direction>,
    },
    #[error("{direction} state has {actual} batches but the plan has {expected}")]
    BatchCountMismatch {
        direction: Direction,
        expected: usize,
        actual: usize,
    },
    #[error("{direction} state has {actual} cells but the grid has {expected}")]
    CellCountMismatch {
        direction: Direction,
        expected: usize,
        actual: usize,
    },
    #[error(
        "{direction} cell {cell_index} to buy back {to_buy_back} exceeds its quantity {cell_quantity}"
    )]
    ToBuyBackAboveCellQuantity {
        direction: Direction,
        cell_index: CellIndex,
        to_buy_back: Quantity,
        cell_quantity: Quantity,
    },
    #[error(
        "{direction} batch {batch_index} filled {filled}, above its core quantity {core_quantity}"
    )]
    FilledAboveCoreQuantity {
        direction: Direction,
        batch_index: BatchIndex,
        filled: Quantity,
        core_quantity: Quantity,
    },
    #[error("{direction} throttle released {released} units but the plan has {unit_count}")]
    ThrottleReleasedAboveUnitCount {
        direction: Direction,
        released: usize,
        unit_count: usize,
    },
}

/// 两侧持仓相加：多头侧为正，空头侧为负。
///
/// 每一侧的持仓都不超过 `M`，正常配置下相加不会越界；真越界了就按可表示范围的端点取值，
/// 不做回绕。
pub(crate) fn net_position(sides: impl Iterator<Item = (Direction, Quantity)>) -> Position {
    let units = sides.fold(0_i128, |total, (direction, quantity)| {
        total + i128::from(quantity.units()) * i128::from(direction.sign())
    });
    let clamped = i64::try_from(units).unwrap_or(if units < 0 { i64::MIN } else { i64::MAX });
    Position::from_units(clamped)
}

#[cfg(test)]
mod tests {
    use super::{BatchState, SideState, StateError, StrategyState};
    use crate::batch::BatchIndex;
    use crate::fixtures::{core_long_strategy, long_grid_strategy};
    use crate::grid::CellIndex;
    use crate::side::Direction;
    use crate::units::{Position, Quantity};

    /// 测试里大多只看多头侧。
    fn long_plan(strategy: &crate::Strategy) -> &crate::side::SidePlan {
        strategy
            .side(Direction::Long)
            .expect("the long side is configured")
    }

    #[test]
    fn expected_position_is_the_core_plus_the_held_cells() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();

        // 主仓成交 .175，格子全部启用但还没建仓：持仓就是主仓。
        state
            .side_mut(Direction::Long)
            .unwrap()
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.175").unwrap();
        strategy.advance(&mut state, &[]);
        assert_eq!(
            state.side(Direction::Long).unwrap().core_position(),
            Quantity::parse("0.175").unwrap()
        );
        assert_eq!(
            state
                .side(Direction::Long)
                .unwrap()
                .grid_position(long_plan(&strategy).grid()),
            Quantity::ZERO
        );
        assert_eq!(
            strategy.expected_position(&state),
            Position::parse("0.175").unwrap()
        );

        // 95、94 两格建仓：各自 .015 计入持仓。
        for index in 0..2 {
            state
                .side_mut(Direction::Long)
                .unwrap()
                .cell_mut(CellIndex(index))
                .unwrap()
                .to_buy_back = Quantity::ZERO;
        }
        assert_eq!(
            state
                .side(Direction::Long)
                .unwrap()
                .grid_position(long_plan(&strategy).grid()),
            Quantity::parse("0.03").unwrap()
        );
        assert_eq!(
            strategy.expected_position(&state),
            Position::parse("0.205").unwrap()
        );

        // 95 格平仓：持仓减回主仓加剩下那一格。
        state
            .side_mut(Direction::Long)
            .unwrap()
            .cell_mut(CellIndex(0))
            .unwrap()
            .to_buy_back = Quantity::parse("0.015").unwrap();
        assert_eq!(
            strategy.expected_position(&state),
            Position::parse("0.19").unwrap()
        );
    }

    #[test]
    fn disabled_cells_hold_nothing_even_with_a_partial_to_buy_back() {
        let strategy = long_grid_strategy();
        let mut state = strategy.initial_state();
        state
            .side_mut(Direction::Long)
            .unwrap()
            .cell_mut(CellIndex(0))
            .unwrap()
            .enabled = false;

        assert_eq!(
            state
                .side(Direction::Long)
                .unwrap()
                .grid_position(long_plan(&strategy).grid()),
            Quantity::ZERO
        );
        assert_eq!(strategy.expected_position(&state), Position::ZERO);
    }

    #[test]
    fn validate_state_rejects_state_that_does_not_match_the_plan() {
        let strategy = core_long_strategy();
        let state = strategy.initial_state();
        assert!(strategy.validate_state(&state).is_ok());

        let long = state.side(Direction::Long).unwrap();
        let fewer_batches = StrategyState::new(vec![SideState::new(
            Direction::Long,
            vec![BatchState::default()],
            long.cells().to_vec(),
        )]);
        assert!(matches!(
            strategy.validate_state(&fewer_batches).unwrap_err(),
            StateError::BatchCountMismatch {
                expected: 4,
                actual: 1,
                ..
            }
        ));

        let fewer_cells = StrategyState::new(vec![SideState::new(
            Direction::Long,
            long.batches().to_vec(),
            vec![],
        )]);
        assert!(matches!(
            strategy.validate_state(&fewer_cells).unwrap_err(),
            StateError::CellCountMismatch {
                expected: 20,
                actual: 0,
                ..
            }
        ));

        // 主仓成交超过 C = .175。
        let mut overfilled = state.clone();
        overfilled
            .side_mut(Direction::Long)
            .unwrap()
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.2").unwrap();
        assert!(matches!(
            strategy.validate_state(&overfilled).unwrap_err(),
            StateError::FilledAboveCoreQuantity { .. }
        ));

        // 待买回量超过格子数量 .015。
        let mut too_much_to_buy_back = state.clone();
        too_much_to_buy_back
            .side_mut(Direction::Long)
            .unwrap()
            .cell_mut(CellIndex(0))
            .unwrap()
            .to_buy_back = Quantity::parse("0.02").unwrap();
        assert!(matches!(
            strategy.validate_state(&too_much_to_buy_back).unwrap_err(),
            StateError::ToBuyBackAboveCellQuantity { .. }
        ));

        // 少了空头侧：状态和配置的侧不一致。
        let missing_side = StrategyState::new(vec![]);
        assert!(matches!(
            strategy.validate_state(&missing_side).unwrap_err(),
            StateError::SideMismatch { .. }
        ));
    }
}
