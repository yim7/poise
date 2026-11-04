//! 首次接管（第 8 节）：标的上已有仓位时，从靠近空隙的一端分给批次和格子。
//!
//! 接管只改起始状态，不下单：分完之后，规划照常按状态算出要挂的单——持有的格子挂出
//! 平仓单，没买完的主仓继续挂买单，与自身平仓单相交的限价按第 6 节下调。

use crate::Strategy;
use crate::batch::Batch;
use crate::grid::GridPlan;
use crate::side::{Direction, SidePlan};
use crate::state::{SideState, StrategyState};
use crate::units::{Position, Quantity, div_ceil};

/// 已有仓位接管不了（第 8 节第 1 条）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TakeoverError {
    #[error(
        "标的上已有{short}仓 {quantity}，但配置里没有{side}",
        short = .direction.short_label(),
        side = .direction.side_label()
    )]
    NoMatchingSide {
        direction: Direction,
        quantity: Quantity,
    },
    #[error(
        "标的上已有{short}仓 {quantity}，超过{side}最大仓位 M {max_position}",
        short = .direction.short_label(),
        side = .direction.side_label()
    )]
    AboveMaxPosition {
        direction: Direction,
        quantity: Quantity,
        max_position: Quantity,
    },
    /// 分完所有单位还剩下一部分：划分本身合计不到 M 时才会出现。
    #[error(
        "标的上已有的{short}仓里有 {left} 分不到批次或格子",
        short = .direction.short_label()
    )]
    Unallocated {
        direction: Direction,
        left: Quantity,
    },
}

impl Strategy {
    /// 接管已有仓位后的起始状态；仓位为零时就是 [`Strategy::initial_state`]。
    ///
    /// 仓位的方向决定交给哪一侧：多单给多头侧，空单给空头侧（中性网格同样如此）。
    /// 这一侧要存在，数量不能超过它的 `M`。分完之后，按状态推算的应有持仓正好等于这份
    /// 仓位，第一步的仓位核对就对得上。
    ///
    /// 分完之后按主仓成交推进一次批次（[`Strategy::advance`]），和运行的第一步看到的是
    /// 同一份状态：主仓分满的批次启用全部格子，剩余不足最低下单金额的批次提前结束。
    ///
    /// 接管不占用限速（第 8 节第 3 条）：已经结束的单位都算走完了，下一个单位按启动时的
    /// 规则排队，也就是立即放行；它之后的单位照常等前一个单位的第一笔成交满 τ。只分到一
    /// 部分、还没结束的单位就是那个「下一个」：它已经放行，后面的单位等它的下一笔成交。
    pub fn takeover_state(&self, position: Position) -> Result<StrategyState, TakeoverError> {
        let mut state = self.initial_state();
        if position.is_zero() {
            return Ok(state);
        }
        let direction = if position.units() > 0 {
            Direction::Long
        } else {
            Direction::Short
        };
        let quantity = Quantity::from_units(position.units().abs());
        let side = self.side(direction).ok_or(TakeoverError::NoMatchingSide {
            direction,
            quantity,
        })?;
        let max_position = side.config().max_position;
        if quantity > max_position {
            return Err(TakeoverError::AboveMaxPosition {
                direction,
                quantity,
                max_position,
            });
        }

        let side_state = state
            .side_mut(direction)
            .expect("the initial state has every configured side");
        let left = if side.batches().is_empty() {
            allocate_cells(side.grid(), side_state, quantity)
        } else {
            self.allocate_batches(side, side_state, quantity)
        };
        if !left.is_zero() {
            return Err(TakeoverError::Unallocated { direction, left });
        }
        self.advance(&mut state, &[]);
        if side.config().throttle_interval.is_some() {
            let side_state = state
                .side_mut(direction)
                .expect("the initial state has every configured side");
            let done = done_units(side, side_state);
            side_state.throttle_mut().released = (done + 1).min(side.unit_count());
        }
        debug_assert_eq!(self.expected_position(&state), position);
        Ok(state)
    }

    /// 有主仓时逐批分配（第 8 节第 2 条），返回没分出去的数量（正常为零）。
    fn allocate_batches(
        &self,
        side: &SidePlan,
        state: &mut SideState,
        quantity: Quantity,
    ) -> Quantity {
        let mut left = quantity;
        for batch in side.batches().batches() {
            if left.is_zero() {
                break;
            }
            // 完整的一批：主仓买入已结束，全部格子持有。
            if left >= batch.quantity {
                fill_batch(
                    side.grid(),
                    state,
                    batch,
                    batch.core_quantity,
                    batch.grid_quantity,
                );
                left = left.saturating_sub(batch.quantity);
                continue;
            }
            // 最后一批只分到一部分：主仓按这一批自己的比例向上取整，其余从靠近批次价的
            // 格子依次填满。
            let core = partial_core(left, batch, self.spec().quantity_step());
            let grid = left.saturating_sub(core);
            return fill_batch(side.grid(), state, batch, core, grid);
        }
        left
    }
}

/// 一批只分到 `left` 时记多少主仓：按这一批的主仓比例 `C ÷ q` 向上取整到数量精度，
/// 不超过 `C`（第 8 节第 2 条）。
///
/// 没有主仓倾斜时每批的比例约等于 `r`（`C` 按最小单位向上取整，略高）；有倾斜时各批的
/// 比例不同，所以一律按批算。接管只在预览和确认启动时做，不影响运行中的策略。
fn partial_core(left: Quantity, batch: &Batch, step: Quantity) -> Quantity {
    if batch.quantity.is_zero() {
        return Quantity::ZERO;
    }
    let units = div_ceil(
        i128::from(left.units()) * i128::from(batch.core_quantity.units()),
        i128::from(batch.quantity.units()),
    );
    let units = div_ceil(units, i128::from(step.units())) * i128::from(step.units());
    Quantity::from_units(i64::try_from(units).unwrap_or(i64::MAX)).min(batch.core_quantity)
}

/// 从启动端起已经走完的单位数：有主仓时是已经结束的批次，没有主仓时是持有满额的格子。
///
/// 走完的单位不会再有让限速计时的成交（结束的批次不再买主仓，满额的格子要等卖出之后
/// 才会再建仓），所以接管之后限速不能等它们。
fn done_units(side: &SidePlan, state: &SideState) -> usize {
    if side.batches().is_empty() {
        side.grid()
            .cells()
            .iter()
            .take_while(|cell| {
                state
                    .cell(cell.index)
                    .is_some_and(|cell_state| cell_state.held(cell.quantity) == cell.quantity)
            })
            .count()
    } else {
        state
            .batches()
            .iter()
            .take_while(|batch| batch.finished)
            .count()
    }
}

/// 给一批记上主仓 `core`，并把 `grid` 从靠近批次价的格子依次填满，返回没填进去的数量。
///
/// 主仓分满的一批记为已结束（第 8 节「视为主仓买入已结束」）。
fn fill_batch(
    grid: &GridPlan,
    state: &mut SideState,
    batch: &Batch,
    core: Quantity,
    mut left: Quantity,
) -> Quantity {
    let batch_state = state
        .batch_mut(batch.index)
        .expect("the initial state has every batch");
    batch_state.filled = core;
    batch_state.finished = core >= batch.core_quantity;
    for cell_index in &batch.cells {
        if left.is_zero() {
            break;
        }
        let cell = grid
            .cell(*cell_index)
            .expect("a batch only lists its cells");
        let held = left.min(cell.quantity);
        let cell_state = state
            .cell_mut(*cell_index)
            .expect("the initial state has every cell");
        cell_state.enabled = true;
        cell_state.to_buy_back = cell.quantity.saturating_sub(held);
        left = left.saturating_sub(held);
    }
    left
}

/// 没有主仓时逐格分配：最后一格只分到一部分时，其余部分记为待买回（第 8 节第 2 条）。
/// 返回没分出去的数量（正常为零）。
fn allocate_cells(grid: &GridPlan, state: &mut SideState, quantity: Quantity) -> Quantity {
    let mut left = quantity;
    for cell in grid.cells() {
        if left.is_zero() {
            break;
        }
        let held = left.min(cell.quantity);
        let cell_state = state
            .cell_mut(cell.index)
            .expect("the initial state has every cell");
        cell_state.to_buy_back = cell.quantity.saturating_sub(held);
        left = left.saturating_sub(held);
    }
    left
}

#[cfg(test)]
mod tests {
    use super::TakeoverError;
    use crate::batch::BatchIndex;
    use crate::fixtures::{
        btc_fees, btc_spec, core_long_config, core_long_strategy, core_short_strategy,
        long_grid_strategy, neutral_grid_strategy, short_grid_config, skewed_core_long_strategy,
    };
    use crate::grid::CellIndex;
    use crate::side::Direction;
    use crate::units::{Position, Quantity};
    use crate::{Strategy, state::StrategyState};
    use chrono::Duration;

    fn quantity(value: &str) -> Quantity {
        Quantity::parse(value).unwrap()
    }

    fn position(value: &str) -> Position {
        Position::parse(value).unwrap()
    }

    /// 一侧各格的（是否启用，持有量）。
    fn cells(
        strategy: &Strategy,
        state: &StrategyState,
        direction: Direction,
    ) -> Vec<(bool, Quantity)> {
        let side = strategy.side(direction).unwrap();
        side.grid()
            .cells()
            .iter()
            .map(|cell| {
                let cell_state = state.side(direction).unwrap().cell(cell.index).unwrap();
                (cell_state.enabled, cell_state.held(cell.quantity))
            })
            .collect()
    }

    #[test]
    fn a_flat_account_starts_from_the_initial_state() {
        let strategy = core_long_strategy();

        assert_eq!(
            strategy.takeover_state(Position::ZERO).unwrap(),
            strategy.initial_state()
        );
    }

    /// B21：设计第 8 节的例子。做多网格 `[75, 95]`（千美元），`M = 1`，`r = .7`，`n = 4`，
    /// 已有 .37 多单。
    #[test]
    fn b21_the_design_example_is_allocated_from_the_gap_end() {
        let strategy = core_long_strategy();

        let state = strategy.takeover_state(position("0.37")).unwrap();

        let long = state.side(Direction::Long).unwrap();
        // 95 批完整：主仓 .175，已结束。
        let first = long.batch(BatchIndex(0)).unwrap();
        assert_eq!(first.filled, quantity("0.175"));
        assert!(first.finished);
        // 90 批分到剩余 .12：主仓 ceil(.7 × .12) = .084，还没买完。
        let second = long.batch(BatchIndex(1)).unwrap();
        assert_eq!(second.filled, quantity("0.084"));
        assert!(!second.finished);
        assert!(!long.batch(BatchIndex(2)).unwrap().finished);
        assert_eq!(long.batch(BatchIndex(2)).unwrap().filled, Quantity::ZERO);

        let held = cells(&strategy, &state, Direction::Long);
        // 95 到 91 五格各持有 .015。
        for cell in &held[0..5] {
            assert_eq!(*cell, (true, quantity("0.015")));
        }
        // 90、89 两格各持有 .015；88 格持有 .006，待买回 .009；87、86 没启用。
        assert_eq!(held[5], (true, quantity("0.015")));
        assert_eq!(held[6], (true, quantity("0.015")));
        assert_eq!(held[7], (true, quantity("0.006")));
        assert_eq!(
            long.cell(CellIndex(7)).unwrap().to_buy_back,
            quantity("0.009")
        );
        assert_eq!(held[8], (false, Quantity::ZERO));
        assert_eq!(held[9], (false, Quantity::ZERO));
        assert!(
            held[10..]
                .iter()
                .all(|cell| *cell == (false, Quantity::ZERO))
        );

        // 应有持仓 = .175 + .075 + .084 + .036 = .37，正好等于实际仓位。
        assert_eq!(strategy.expected_position(&state), position("0.37"));
        strategy.validate_state(&state).unwrap();
    }

    /// 有主仓倾斜时最后一批按它自己的比例拆：90 批 q = .221、C = .146。
    #[test]
    fn a_partial_batch_is_split_by_its_own_core_ratio_when_the_core_is_skewed() {
        let strategy = skewed_core_long_strategy();

        let state = strategy.takeover_state(position("0.3")).unwrap();

        let long = state.side(Direction::Long).unwrap();
        // 95 批完整（.163）：主仓 .088，已结束。
        let first = long.batch(BatchIndex(0)).unwrap();
        assert_eq!(first.filled, quantity("0.088"));
        assert!(first.finished);
        // 90 批分到剩余 .137：主仓 ceil(.137 × .146 / .221) = .091，网格 .046。
        let second = long.batch(BatchIndex(1)).unwrap();
        assert_eq!(second.filled, quantity("0.091"));
        assert!(!second.finished);

        let held = cells(&strategy, &state, Direction::Long);
        for cell in &held[0..5] {
            assert_eq!(*cell, (true, quantity("0.015")));
        }
        // 90、89、88 三格各持有 .015；87 格持有 .001，待买回 .014；86 没启用。
        for cell in &held[5..8] {
            assert_eq!(*cell, (true, quantity("0.015")));
        }
        assert_eq!(held[8], (true, quantity("0.001")));
        assert_eq!(
            long.cell(CellIndex(8)).unwrap().to_buy_back,
            quantity("0.014")
        );
        assert_eq!(held[9], (false, Quantity::ZERO));

        assert_eq!(strategy.expected_position(&state), position("0.3"));
        strategy.validate_state(&state).unwrap();
    }

    /// 没有主仓时逐格分配，最后一格只持有一部分。
    #[test]
    fn without_a_core_the_cells_are_filled_one_by_one() {
        let strategy = long_grid_strategy();

        let state = strategy.takeover_state(position("0.12")).unwrap();

        let held = cells(&strategy, &state, Direction::Long);
        assert_eq!(held[0], (true, quantity("0.05")));
        assert_eq!(held[1], (true, quantity("0.05")));
        assert_eq!(held[2], (true, quantity("0.02")));
        assert_eq!(held[3], (true, Quantity::ZERO));
        assert_eq!(strategy.expected_position(&state), position("0.12"));
    }

    /// 空单交给空头侧，同样从靠近空隙的一端（低价一端）开始。
    #[test]
    fn a_short_position_goes_to_the_short_side() {
        let strategy = core_short_strategy();

        let state = strategy.takeover_state(position("-0.37")).unwrap();

        let short = state.side(Direction::Short).unwrap();
        assert_eq!(
            short.batch(BatchIndex(0)).unwrap().filled,
            quantity("0.175")
        );
        assert_eq!(
            short.batch(BatchIndex(1)).unwrap().filled,
            quantity("0.084")
        );
        assert_eq!(strategy.expected_position(&state), position("-0.37"));
    }

    /// 中性网格：仓位方向决定交给哪一侧，另一侧不动。
    #[test]
    fn a_neutral_grid_takes_the_position_on_the_matching_side() {
        let strategy = neutral_grid_strategy();

        let state = strategy.takeover_state(position("-0.07")).unwrap();

        let short = cells(&strategy, &state, Direction::Short);
        assert_eq!(short[0], (true, quantity("0.05")));
        assert_eq!(short[1], (true, quantity("0.02")));
        assert_eq!(
            state.side(Direction::Long),
            strategy.initial_state().side(Direction::Long)
        );
        assert_eq!(strategy.expected_position(&state), position("-0.07"));
    }

    /// B22：仓位方向没有对应的一侧时拒绝。
    #[test]
    fn b22_a_position_without_a_matching_side_is_refused() {
        let strategy = Strategy::new(short_grid_config(), btc_spec(), btc_fees()).unwrap();

        assert_eq!(
            strategy.takeover_state(position("0.1")),
            Err(TakeoverError::NoMatchingSide {
                direction: Direction::Long,
                quantity: quantity("0.1"),
            })
        );
    }

    /// B22：数量超过该侧上限时拒绝。
    #[test]
    fn b22_a_position_above_the_max_position_is_refused() {
        let strategy = core_long_strategy();

        assert_eq!(
            strategy.takeover_state(position("1.001")),
            Err(TakeoverError::AboveMaxPosition {
                direction: Direction::Long,
                quantity: quantity("1.001"),
                max_position: quantity("1"),
            })
        );
    }

    /// 正好等于 M：全部单位都分满，没有剩余。
    #[test]
    fn a_full_position_fills_every_unit() {
        let strategy = core_long_strategy();

        let state = strategy.takeover_state(position("1")).unwrap();

        let long = state.side(Direction::Long).unwrap();
        assert!(long.batches().iter().all(|batch| batch.finished));
        assert_eq!(strategy.expected_position(&state), position("1"));
    }

    /// 接管之后按主仓成交推进一次批次，和运行的第一步看到同一份状态：主仓分满的批次
    /// 启用全部格子，预览列得出第一步会挂的格子买单。
    #[test]
    fn a_batch_whose_core_is_complete_enables_its_cells_right_away() {
        let mut config = core_long_config();
        config.long.as_mut().unwrap().core_ratio = crate::units::Rate::parse("0.001").unwrap();
        let strategy = Strategy::new(config, btc_spec(), btc_fees()).unwrap();

        // 第一批主仓 C = ceil(.001 × .25) = .001：接管 .001 正好分满主仓，格子一格没分到。
        let state = strategy.takeover_state(position("0.001")).unwrap();

        let long = state.side(Direction::Long).unwrap();
        assert!(long.batch(BatchIndex(0)).unwrap().finished);
        let held = cells(&strategy, &state, Direction::Long);
        assert!(
            held[0..5]
                .iter()
                .all(|(enabled, held)| *enabled && held.is_zero())
        );
        assert_eq!(strategy.expected_position(&state), position("0.001"));
    }

    /// 主仓只差一点、不够最低下单金额的批次在接管时就提前结束；限速不能等它，否则它不会
    /// 再有主仓成交，后面的批次永远放不开。
    #[test]
    fn a_batch_ended_by_a_tiny_remainder_does_not_hold_back_the_throttle() {
        let mut config = core_long_config();
        config.long.as_mut().unwrap().throttle_interval = Some(Duration::hours(1));
        let strategy = Strategy::new(config, crate::fixtures::fine_spec(), btc_fees()).unwrap();

        // 第一批主仓记 ceil(.7 × .24997) = .17498，还差 .00002，按 95000 只有 1.9 美元。
        let state = strategy.takeover_state(position("0.24997")).unwrap();

        let long = state.side(Direction::Long).unwrap();
        assert_eq!(
            long.batch(BatchIndex(0)).unwrap().filled,
            quantity("0.17498")
        );
        assert!(long.batch(BatchIndex(0)).unwrap().finished);
        assert_eq!(long.throttle().released, 2, "第二批立即放行");
        assert_eq!(strategy.expected_position(&state), position("0.24997"));
    }

    /// B13b：接管 .37 后价格在 88，持有格子的卖单留在平仓价 96…89，90 批补主仓的买单从
    /// 90 下调到 88.999（自身最低卖价 89 下方一个报价单位），仍能买到外部低于 89 的卖盘。
    #[test]
    fn b13b_the_core_buy_after_a_takeover_steps_below_the_own_sells() {
        let strategy = core_long_strategy();
        let state = strategy.takeover_state(position("0.37")).unwrap();

        let plan = strategy.plan(
            &state,
            &[],
            &[],
            crate::units::Price::parse("88000").unwrap(),
        );

        let core = plan
            .place
            .iter()
            .find(|intent| intent.purpose == crate::order::OrderPurpose::Core)
            .unwrap();
        assert_eq!(
            core.limit_price,
            crate::units::Price::parse("88999").unwrap()
        );
        assert_eq!(core.quantity, quantity("0.091"));
        let exits: Vec<crate::units::Price> = plan
            .place
            .iter()
            .filter(|intent| intent.purpose == crate::order::OrderPurpose::GridExit)
            .map(|intent| intent.limit_price)
            .collect();
        assert_eq!(
            exits,
            [
                "96000", "95000", "94000", "93000", "92000", "91000", "90000", "89000"
            ]
            .map(|price| crate::units::Price::parse(price).unwrap())
            .to_vec()
        );
    }

    /// 接管不占用限速：分完的单位算已经开始，下一个单位立即放行（第 8 节第 3 条）。
    #[test]
    fn the_takeover_does_not_use_up_the_throttle() {
        let mut config = core_long_config();
        config.long.as_mut().unwrap().throttle_interval = Some(Duration::hours(1));
        let strategy = Strategy::new(config, btc_spec(), btc_fees()).unwrap();

        // 只分满第一批：第二批立即放行。
        let state = strategy.takeover_state(position("0.25")).unwrap();
        assert_eq!(state.side(Direction::Long).unwrap().throttle().released, 2);

        // 第二批只分到一部分：它已经放行，第三批等它的下一笔主仓成交满 τ。
        let state = strategy.takeover_state(position("0.37")).unwrap();
        assert_eq!(state.side(Direction::Long).unwrap().throttle().released, 2);
    }
}
