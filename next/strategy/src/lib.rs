//! 策略计算：纯函数，不含任何 I/O。
//!
//! 不读时钟、不读写文件、不访问网络。所有输入（状态、在场订单、当前价格、时间）
//! 都由调用方传入，所有输出都是可以再算一遍的值。
//!
//! 本任务支持的配置：做多侧、没有主仓（主仓比例为 0）、不限速。
//! 主仓批次、限速、做空与中性、接管、观点失效价属于后续任务。

pub mod config;
pub mod grid;
pub mod instrument;
pub mod ledger;
pub mod order;
pub mod planning;
pub mod state;
pub mod units;

#[cfg(test)]
pub(crate) mod fixtures;

use crate::config::{ConfigError, NormalizedConfig, StrategyConfig};
use crate::grid::GridPlan;
use crate::instrument::{Fees, InstrumentSpec};
use crate::state::{StateError, StrategyState};

/// 一个已经规整并校验过的策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strategy {
    config: StrategyConfig,
    spec: InstrumentSpec,
    fees: Fees,
    grid: GridPlan,
}

impl Strategy {
    /// 规整配置、划分格子并校验。
    pub fn new(
        config: StrategyConfig,
        spec: InstrumentSpec,
        fees: Fees,
    ) -> Result<Self, ConfigError> {
        let NormalizedConfig { config, grid } = config.normalize(&spec, &fees)?;
        Ok(Self {
            config,
            spec,
            fees,
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

    pub fn grid(&self) -> &GridPlan {
        &self.grid
    }

    /// 刚启动时的状态：没有主仓时每格自成单位，全部启用、全部待买回。
    pub fn initial_state(&self) -> StrategyState {
        StrategyState::new(
            self.grid
                .cells()
                .iter()
                .map(|cell| crate::state::CellState::enabled_all_to_buy_back(cell.quantity))
                .collect(),
        )
    }

    /// 检查保存的状态与格子划分是否一致。
    pub fn validate_state(&self, state: &StrategyState) -> Result<(), StateError> {
        if state.cells().len() != self.grid.len() {
            return Err(StateError::CellCountMismatch {
                expected: self.grid.len(),
                actual: state.cells().len(),
            });
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
    pub fn expected_position(&self, state: &StrategyState) -> crate::units::Position {
        state.expected_position(&self.grid)
    }
}

#[cfg(test)]
mod tests {
    use crate::fixtures::{btc_fees, btc_spec, long_grid_config};
    use crate::state::StateError;
    use crate::units::{Position, Quantity};
    use crate::{Strategy, state::StrategyState};

    #[test]
    fn initial_state_holds_every_cell_and_matches_the_expected_position() {
        let strategy = Strategy::new(long_grid_config(), btc_spec(), btc_fees()).unwrap();

        let state = strategy.initial_state();

        assert_eq!(state.cells().len(), 20);
        assert!(state.cells().iter().all(|cell| cell.enabled));
        assert_eq!(strategy.expected_position(&state), Position::ZERO);
        assert!(strategy.validate_state(&state).is_ok());
    }

    #[test]
    fn validate_state_rejects_states_that_do_not_match_the_grid() {
        let strategy = Strategy::new(long_grid_config(), btc_spec(), btc_fees()).unwrap();

        let short = StrategyState::new(Vec::new());
        assert!(matches!(
            strategy.validate_state(&short).unwrap_err(),
            StateError::CellCountMismatch { .. }
        ));

        let mut state = strategy.initial_state();
        state
            .cell_mut(crate::grid::CellIndex(0))
            .unwrap()
            .to_buy_back = Quantity::parse("0.06").unwrap();
        assert!(matches!(
            strategy.validate_state(&state).unwrap_err(),
            StateError::ToBuyBackAboveCellQuantity { .. }
        ));
    }
}
