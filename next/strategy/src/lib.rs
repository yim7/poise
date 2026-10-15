//! 策略计算：纯函数，不含任何 I/O。
//!
//! 不读时钟、不读写文件、不访问网络。所有输入（状态、在场订单、当前价格、时间）
//! 都由调用方传入，所有输出都是可以再算一遍的值。
//!
//! 本任务支持的配置：做多侧，可选限速。主仓比例大于 0 时按批次买入主仓、格子跟随
//! 所属批次按成交比例启用；比例为 0 时每个格子自成单位。
//! 做空与中性、接管、观点失效价属于后续任务。

pub mod batch;
pub mod config;
pub mod grid;
pub mod instrument;
pub mod ledger;
pub mod order;
pub mod planning;
pub mod preview;
pub mod state;
pub mod throttle;
pub mod units;

#[cfg(test)]
pub(crate) mod fixtures;

use crate::batch::{Batch, BatchPlan};
use crate::config::{ConfigError, NormalizedConfig, StrategyConfig};
use crate::grid::GridPlan;
use crate::instrument::{Fees, InstrumentSpec};
use crate::order::{OrderPurpose, Unit, WorkingOrder};
use crate::state::{BatchState, CellState, StateError, StrategyState};
use crate::throttle::ThrottleState;
use crate::units::{Position, Quantity, div_floor, div_round};

/// 一个已经规整并校验过的策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strategy {
    config: StrategyConfig,
    spec: InstrumentSpec,
    fees: Fees,
    batches: BatchPlan,
    grid: GridPlan,
}

impl Strategy {
    /// 规整配置、划分批次与格子并校验。
    pub fn new(
        config: StrategyConfig,
        spec: InstrumentSpec,
        fees: Fees,
    ) -> Result<Self, ConfigError> {
        let NormalizedConfig {
            config,
            batches,
            grid,
        } = config.normalize(&spec, &fees)?;
        Ok(Self {
            config,
            spec,
            fees,
            batches,
            grid,
        })
    }

    /// 规整后的配置。
    pub fn config(&self) -> &StrategyConfig {
        &self.config
    }

    pub fn spec(&self) -> &InstrumentSpec {
        &self.spec
    }

    pub fn fees(&self) -> &Fees {
        &self.fees
    }

    /// 批次划分；主仓比例为 0 时没有批次。
    pub fn batches(&self) -> &BatchPlan {
        &self.batches
    }

    pub fn grid(&self) -> &GridPlan {
        &self.grid
    }

    /// 刚启动时的状态。
    ///
    /// 没有主仓时每格自成单位，全部启用、全部待买回；有主仓时格子全部未启用，
    /// 要等所属批次的主仓开始成交后才逐个启用。
    ///
    /// 限速进度：不限速时全部单位都已放行；限速时只有第一个单位放行（第 4.1 节），
    /// 它的放行时刻由调用方在启动时用 [`Strategy::advance_throttle`] 记下。
    pub fn initial_state(&self) -> StrategyState {
        let mut state = StrategyState::new(
            self.batches
                .batches()
                .iter()
                .map(|_| BatchState::default())
                .collect(),
            self.grid
                .cells()
                .iter()
                .map(|cell| CellState {
                    enabled: cell.batch.is_none(),
                    to_buy_back: cell.quantity,
                })
                .collect(),
        );
        let released = match self.throttle_interval() {
            Some(_) => self.unit_count().min(1),
            None => self.unit_count(),
        };
        state.set_throttle(ThrottleState {
            released,
            ..ThrottleState::default()
        });
        state
    }

    /// 检查保存的状态与批次、格子划分是否一致。
    pub fn validate_state(&self, state: &StrategyState) -> Result<(), StateError> {
        if state.batches().len() != self.batches.len() {
            return Err(StateError::BatchCountMismatch {
                expected: self.batches.len(),
                actual: state.batches().len(),
            });
        }
        if state.cells().len() != self.grid.len() {
            return Err(StateError::CellCountMismatch {
                expected: self.grid.len(),
                actual: state.cells().len(),
            });
        }
        // 放行数只会随单位总数一起增长，超出来说明状态不是这份配置产生的。
        if state.throttle().released > self.unit_count() {
            return Err(StateError::ThrottleReleasedAboveUnitCount {
                released: state.throttle().released,
                unit_count: self.unit_count(),
            });
        }
        for batch in self.batches.batches() {
            let Some(batch_state) = state.batch(batch.index) else {
                return Err(StateError::BatchCountMismatch {
                    expected: self.batches.len(),
                    actual: state.batches().len(),
                });
            };
            if batch_state.filled.units() < 0 || batch_state.filled > batch.core_quantity {
                return Err(StateError::FilledAboveCoreQuantity {
                    batch_index: batch.index,
                    filled: batch_state.filled,
                    core_quantity: batch.core_quantity,
                });
            }
        }
        for cell in self.grid.cells() {
            let Some(cell_state) = state.cell(cell.index) else {
                return Err(StateError::CellCountMismatch {
                    expected: self.grid.len(),
                    actual: state.cells().len(),
                });
            };
            if cell_state.to_buy_back.units() < 0 || cell_state.to_buy_back > cell.quantity {
                return Err(StateError::ToBuyBackAboveCellQuantity {
                    cell_index: cell.index,
                    to_buy_back: cell_state.to_buy_back,
                    cell_quantity: cell.quantity,
                });
            }
        }
        Ok(())
    }

    /// 按批次和格子推算的应有持仓。做多时为正。
    pub fn expected_position(&self, state: &StrategyState) -> Position {
        state.expected_position(&self.grid)
    }

    /// 按主仓成交推进批次：启用格子、判断本批是否提前结束。返回状态是否有变化。
    ///
    /// 纯计算，不读写任何外部状态；调用方在记账之后、规划之前调用它。
    /// - 启用格子：从靠近批次价的一端开始，至少 `floor(格子数 × F / C)` 格（第 4.2.2 节）；
    /// - 结束：`F` 达到 `C` 时启用本批全部格子；剩余量按批次价不足最低下单金额时
    ///   提前结束，按四舍五入启用 `round(格子数 × F / C)` 格（第 4.2.3 节）。
    pub fn advance(&self, state: &mut StrategyState, working_orders: &[WorkingOrder]) -> bool {
        let mut changed = false;
        for batch in self.batches.batches() {
            let Some(batch_state) = state.batch(batch.index) else {
                continue;
            };
            let filled = batch_state.filled.min(batch.core_quantity);
            let finished = batch_state.finished;
            let core_units = batch.core_quantity.units() as i128;
            let cells = batch.cells.len() as i128;
            let enabled_by_ratio = div_floor(cells * filled.units() as i128, core_units) as usize;
            changed |= self.enable_batch_cells(state, batch, enabled_by_ratio);

            if finished {
                continue;
            }
            if filled >= batch.core_quantity {
                self.mark_finished(state, batch);
                changed = true;
                continue;
            }
            let outstanding = outstanding_core(batch, filled, working_orders);
            if outstanding.is_zero() {
                continue;
            }
            if !self.spec.meets_minimum_order(outstanding, batch.price) {
                // 剩余量按批次价就不足最低下单金额：本批提前结束，其余格子不再启用。
                let rounded = div_round(cells * filled.units() as i128, core_units) as usize;
                self.enable_batch_cells(state, batch, rounded);
                self.mark_finished(state, batch);
                changed = true;
            }
        }
        changed
    }

    /// 启用一批里从靠近批次价一端开始的 `target` 个格子；已启用的格子不会关闭。
    fn enable_batch_cells(&self, state: &mut StrategyState, batch: &Batch, target: usize) -> bool {
        let mut changed = false;
        for cell_index in batch.cells.iter().take(target) {
            let quantity = self
                .grid
                .cell(*cell_index)
                .map(|cell| cell.quantity)
                .unwrap_or(Quantity::ZERO);
            if let Some(cell_state) = state.cell_mut(*cell_index)
                && !cell_state.enabled
            {
                cell_state.enabled = true;
                cell_state.to_buy_back = quantity;
                changed = true;
            }
        }
        changed
    }

    fn mark_finished(&self, state: &mut StrategyState, batch: &Batch) {
        if let Some(batch_state) = state.batch_mut(batch.index) {
            batch_state.finished = true;
        }
    }
}

/// 本批还没挂出去的主仓数量：`C − F` 再减去本批在场主仓买单的余量。
///
/// 只有这部分才需要新订单；它不足最低下单金额时本批无法继续买入（第 4.2.3 节）。
pub(crate) fn outstanding_core(
    batch: &Batch,
    filled: Quantity,
    working_orders: &[WorkingOrder],
) -> Quantity {
    let reserved = working_orders
        .iter()
        .filter(|order| {
            order.unit == Unit::Batch(batch.index) && order.purpose == OrderPurpose::CoreBuy
        })
        .fold(Quantity::ZERO, |total, order| total + order.remaining);
    batch
        .core_quantity
        .saturating_sub(filled)
        .saturating_sub(reserved)
}

#[cfg(test)]
mod tests {
    use super::Strategy;
    use crate::batch::BatchIndex;
    use crate::fixtures::{
        at, core_long_strategy, fine_core_long_strategy, single_small_batch_strategy,
    };
    use crate::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Unit};
    use crate::state::StrategyState;
    use crate::units::Quantity;

    /// 记一笔本批主仓的成交，模拟主仓买单部分成交。
    fn fill_core(strategy: &Strategy, state: &mut StrategyState, batch: usize, quantity: &str) {
        let batch_plan = strategy.batches().batch(BatchIndex(batch)).unwrap();
        let mut order = OrderRecord {
            client_order_id: ClientOrderId::try_new("0x01").unwrap(),
            unit: Unit::Batch(BatchIndex(batch)),
            purpose: OrderPurpose::CoreBuy,
            limit_price: batch_plan.price,
            quantity: batch_plan.core_quantity,
            cumulative_filled: Quantity::ZERO,
            reduce_only: false,
            status: OrderStatus::Open,
        };
        strategy.apply_fill(state, &mut order, Quantity::parse(quantity).unwrap(), at(0));
    }

    #[test]
    fn b04_cells_enable_in_proportion_to_the_core_fill() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();
        assert!(state.cells().iter().all(|cell| !cell.enabled));

        fill_core(&strategy, &mut state, 0, "0.07");
        strategy.advance(&mut state, &[]);

        // floor(5 × .07 / .175) = 2：95、94 两格启用。
        let enabled: Vec<usize> = state
            .cells()
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.enabled)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(enabled, vec![0, 1]);
        assert_eq!(
            state.batch(BatchIndex(0)).unwrap().filled,
            Quantity::parse("0.07").unwrap()
        );
        assert!(!state.batch(BatchIndex(0)).unwrap().finished);
        assert_eq!(
            state.cell(crate::grid::CellIndex(1)).unwrap().to_buy_back,
            Quantity::parse("0.015").unwrap()
        );
    }

    #[test]
    fn b04_cells_enable_step_by_step_and_never_disable() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();

        fill_core(&strategy, &mut state, 0, "0.07");
        assert!(strategy.advance(&mut state, &[]));
        fill_core(&strategy, &mut state, 0, "0.035");
        assert!(strategy.advance(&mut state, &[]));

        // floor(5 × .105 / .175) = 3。
        assert!(state.cells()[..3].iter().all(|cell| cell.enabled));
        assert!(state.cells()[3..].iter().all(|cell| !cell.enabled));
        // 再算一遍没有变化。
        assert!(!strategy.advance(&mut state, &[]));
        assert!(state.cells()[..3].iter().all(|cell| cell.enabled));
    }

    #[test]
    fn b05_batch_finishes_early_when_the_remainder_is_below_the_minimum_order_amount() {
        let strategy = fine_core_long_strategy();
        let mut state = strategy.initial_state();

        // 剩余 .0001 × 95000 = 9.5 美元，不足最低金额。
        fill_core(&strategy, &mut state, 0, "0.1749");
        strategy.advance(&mut state, &[]);

        assert!(state.batch(BatchIndex(0)).unwrap().finished);
        // 按四舍五入启用 round(5 × .1749 / .175) = 5 格。
        assert!(state.cells()[..5].iter().all(|cell| cell.enabled));
        // 提前结束不阻挡下一批。
        assert!(!state.batch(BatchIndex(1)).unwrap().finished);
        assert!(state.cells()[5..10].iter().all(|cell| !cell.enabled));
    }

    #[test]
    fn b05_the_remainder_is_measured_against_the_batch_price() {
        let strategy = fine_core_long_strategy();
        let mut state = strategy.initial_state();

        // 剩余 .0001 × 95000 = 9.5 美元：不足最低金额，提前结束。
        fill_core(&strategy, &mut state, 0, "0.1749");
        strategy.advance(&mut state, &[]);
        assert!(state.batch(BatchIndex(0)).unwrap().finished);

        // 剩余 .0002 × 95000 = 19 美元：按批次价够，不结束。
        let mut other = strategy.initial_state();
        fill_core(&strategy, &mut other, 0, "0.1748");
        strategy.advance(&mut other, &[]);
        assert!(!other.batch(BatchIndex(0)).unwrap().finished);
        // floor(5 × .1748 / .175) = 4。
        assert!(other.cells()[..4].iter().all(|cell| cell.enabled));
        assert!(!other.cells()[4].enabled);
    }

    #[test]
    fn b05b_early_finish_enables_the_rounded_number_of_cells_even_when_it_is_none() {
        let strategy = single_small_batch_strategy();
        let mut state = strategy.initial_state();
        let batch = strategy.batches().batch(BatchIndex(0)).unwrap();
        assert_eq!(batch.core_quantity, Quantity::parse("0.11").unwrap());
        assert_eq!(batch.grid_quantity, Quantity::parse("0.39").unwrap());

        // 剩余 .09 × 100 = 9 美元，不足最低金额。
        fill_core(&strategy, &mut state, 0, "0.02");
        strategy.advance(&mut state, &[]);

        assert!(state.batch(BatchIndex(0)).unwrap().finished);
        // round(1 × .02 / .11) = 0：这一格不启用。
        assert!(!state.cells()[0].enabled);
    }

    #[test]
    fn b05_a_finished_batch_stays_finished() {
        let strategy = fine_core_long_strategy();
        let mut state = strategy.initial_state();

        fill_core(&strategy, &mut state, 0, "0.1749");
        strategy.advance(&mut state, &[]);
        // 尾量被撤销后不再补挂：本批已经结束，成交也不会把它改回去。
        assert!(!strategy.advance(&mut state, &[]));
        assert!(state.batch(BatchIndex(0)).unwrap().finished);
    }
}
