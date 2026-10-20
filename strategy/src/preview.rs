//! 预览数据：批次表与启动后会立即成交的订单（第 2.4、4.6 节）。
//!
//! 纯计算，供网页预览使用。两侧各按自己的启动顺序推算：主仓建仓单挂在批次价上，
//! 批次价已经到达当前价时它立即成交；这一批的格子因此全部启用，格子建仓价也已经
//! 到达当前价的那些格子随即建仓。开启限速时启动只放行第一个单位，其余单位不会挂单，
//! 也就不会立即成交（第 4.1 节）。

use serde::{Deserialize, Serialize};

use crate::Strategy;
use crate::batch::BatchIndex;
use crate::grid::{Cell, CellIndex};
use crate::order::{OrderPurpose, Side, Unit, WorkingOrder};
use crate::planning::OrderIntent;
use crate::side::{Direction, SidePlan};
use crate::state::{CellState, StrategyState};
use crate::units::{Notional, Position, Price, Quantity};

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

/// 一个格子在格子表里的状态（第 11 节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellStatus {
    /// 等待所属批次的主仓买入后启用。
    WaitingForBatch,
    /// 已启用，还没有买入。
    WaitingForBuy,
    /// 持有中，等价格到卖价。
    Holding,
    /// 卖出了一部分，等待买回。
    ToBuyBack,
}

/// 格子表的一行：预览和运行中查看都用它。
///
/// 收益是「整格买卖一次」的推算，和已经卖出的比例无关：它说明的是这一格值不值得挂，
/// 不是已经赚到多少。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellSummary {
    /// 属于哪一侧。
    pub direction: Direction,
    pub index: CellIndex,
    /// 跟随的批次；`r = 0` 时没有批次。
    pub batch: Option<BatchIndex>,
    /// 建仓价：多头侧的买价，空头侧的开空价。
    pub entry_price: Price,
    /// 平仓价：多头侧的卖价，空头侧的买平价。
    pub exit_price: Price,
    /// 整格数量。
    pub quantity: Quantity,
    /// 持有量：已经买回、还没卖出的部分。
    pub held: Quantity,
    /// 待买回量 `S`。
    pub to_buy_back: Quantity,
    /// 属于这一格的在场订单张数。
    pub working_orders: usize,
    pub status: CellStatus,
    /// 整格买卖一次、两条腿都按 maker 成交的扣费后收益。
    pub profit_maker: Notional,
    /// 整格买卖一次、两条腿都按 taker 成交的扣费后收益。
    pub profit_taker: Notional,
}

/// 一个格子现在处于哪一步。
///
/// 按持有量分：「等待所属批次」优先于持有量，没启用的格子本来就没有持有，状态要说明
/// 它在等谁；启用之后持有量是零说明整格还没买进来，持有量满格说明整格都在手上，
/// 中间那段是卖出后等着买回的部分。
fn cell_status(cell: &CellState, held: Quantity, quantity: Quantity) -> CellStatus {
    if !cell.enabled {
        return CellStatus::WaitingForBatch;
    }
    if held.is_zero() {
        return CellStatus::WaitingForBuy;
    }
    if held >= quantity {
        return CellStatus::Holding;
    }
    CellStatus::ToBuyBack
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

/// 预览和详情要的那几张表：批次、格子、以及启动后会立即成交的订单。
impl Strategy {
    /// 两侧的批次表，按 [`Direction::ALL`] 的顺序。`state` 为空状态时就是启动时的样子。
    pub fn batch_summaries(&self, state: &StrategyState) -> Vec<BatchSummary> {
        self.sides()
            .iter()
            .flat_map(|side| batch_summaries_of(side, state))
            .collect()
    }

    /// 格子表，两侧按 [`Direction::ALL`] 的顺序接在一起。`working` 是这个策略当前的在场订单。
    pub fn cell_summaries(
        &self,
        state: &StrategyState,
        working: &[WorkingOrder],
    ) -> Vec<CellSummary> {
        self.sides()
            .iter()
            .flat_map(|side| self.cell_summaries_of(side, state, working))
            .collect()
    }

    /// 一侧的格子表。
    fn cell_summaries_of(
        &self,
        side: &SidePlan,
        state: &StrategyState,
        working: &[WorkingOrder],
    ) -> Vec<CellSummary> {
        let direction = side.direction();
        let side_state = state.side(direction);
        side.grid()
            .cells()
            .iter()
            .map(|cell| {
                let cell_state = side_state
                    .and_then(|state| state.cell(cell.index))
                    .cloned()
                    .unwrap_or_default();
                let held = cell_state.held(cell.quantity);
                CellSummary {
                    direction,
                    index: cell.index,
                    batch: cell.batch,
                    entry_price: cell.entry_price,
                    exit_price: cell.exit_price,
                    quantity: cell.quantity,
                    held,
                    // 没启用的格子还没有开始买卖，待买回量是状态里的初始值，
                    // 展示上没有意义，按零报。
                    to_buy_back: if cell_state.enabled {
                        cell_state.to_buy_back
                    } else {
                        Quantity::ZERO
                    },
                    working_orders: working
                        .iter()
                        .filter(|order| order.unit == Unit::Cell(direction, cell.index))
                        .count(),
                    status: cell_status(&cell_state, held, cell.quantity),
                    profit_maker: self.cell_profit(
                        direction,
                        cell.entry_price,
                        cell.exit_price,
                        cell.quantity,
                        self.fees().maker_rate,
                    ),
                    profit_taker: self.cell_profit(
                        direction,
                        cell.entry_price,
                        cell.exit_price,
                        cell.quantity,
                        self.fees().taker_rate,
                    ),
                }
            })
            .collect()
    }

    /// 网格已实现利润的估算（第 7.6 节）：每个格子平仓腿已经成交的数量，按这一格的计划
    /// 建仓价、平仓价和 maker 费率算扣费后收益，加起来。
    ///
    /// 是估算：实际成交价可能比计划价好（下单即成交时），手续费也可能有一条腿是 taker。
    /// 它和预览里「每格扣费后收益」是同一个口径，用来看资金费占了多少利润。
    /// `exits` 是每个单位平仓腿已经记入的成交量；不是格子的单位不算。
    pub fn grid_realized_profit(&self, exits: &[(Unit, Quantity)]) -> Notional {
        exits
            .iter()
            .filter_map(|(unit, quantity)| {
                let Unit::Cell(direction, index) = *unit else {
                    return None;
                };
                let cell = self.side(direction)?.grid().cell(index)?;
                Some(self.cell_profit(
                    direction,
                    cell.entry_price,
                    cell.exit_price,
                    *quantity,
                    self.fees().maker_rate,
                ))
            })
            .fold(Notional::ZERO, |total, profit| total + profit)
    }

    /// 整格买卖一次的扣费后收益：`|平仓价 − 建仓价| × 数量 − (平仓价 + 建仓价) × 数量 × 费率`。
    ///
    /// 多头侧买进卖出、空头侧卖出买回，买卖两条腿对调；两侧的收益算法相同。
    fn cell_profit(
        &self,
        direction: Direction,
        entry_price: Price,
        exit_price: Price,
        quantity: Quantity,
        fee_rate: crate::units::Rate,
    ) -> Notional {
        let (bought, sold) = match direction {
            Direction::Long => (entry_price, exit_price),
            Direction::Short => (exit_price, entry_price),
        };
        let bought = Notional::of(quantity, bought);
        let sold = Notional::of(quantity, sold);
        (sold - bought) - (sold + bought).mul_rate(fee_rate)
    }

    /// 从 `state` 启动后会立即成交的订单，两侧按 [`Direction::ALL`] 的顺序接在一起。
    ///
    /// `state` 是启动时的状态：没有已有仓位时是 [`Strategy::initial_state`]，接管已有仓位时
    /// 是 [`Strategy::takeover_state`]。
    ///
    /// 按第一步实际规划出的订单判断，不按原价：限价可能已经避开本策略自己的反向订单
    /// （第 6 节），够不上最低金额的单根本不挂——原价能成交的，避让后的价格未必。
    pub fn immediate_fills(
        &self,
        state: &StrategyState,
        market_price: Price,
    ) -> Vec<ImmediateFill> {
        let planned = self.plan(state, &[], &[], market_price).place;
        self.sides()
            .iter()
            .flat_map(|side| self.side_immediate_fills(side, state, &planned, market_price))
            .collect()
    }

    /// 一侧启动后会立即成交的订单。
    ///
    /// 第一步挂出的单里，限价已经到达当前价的会立即成交。主仓建仓单成交时假设整批成交
    /// （限价单挂在批次价上，成交即全部成交），这一批的格子随之全部启用：原来没启用的格子
    /// 第一步还没挂单，按建仓价判断，到了当前价的也会立即建仓。
    ///
    /// 开启限速时启动只放行第一个单位（第 4.1 节），后面那些还没放行的单位这一步根本
    /// 不会挂单，所以也不会立即成交。
    fn side_immediate_fills(
        &self,
        side: &SidePlan,
        state: &StrategyState,
        planned: &[OrderIntent],
        market_price: Price,
    ) -> Vec<ImmediateFill> {
        let direction = side.direction();
        let side_state = state.side(direction);
        let enabled = |cell: &Cell| {
            side_state
                .and_then(|side| side.cell(cell.index))
                .is_some_and(|cell_state| cell_state.enabled)
        };
        let planned_cell = |cell: &Cell| {
            let unit = Unit::Cell(direction, cell.index);
            [OrderPurpose::GridEntry, OrderPurpose::GridExit]
                .into_iter()
                .filter_map(move |purpose| planned_fill(planned, unit, purpose, market_price))
        };
        let mut fills = Vec::new();
        if side.batches().is_empty() {
            for cell in side.grid().cells() {
                fills.extend(planned_cell(cell));
            }
            return fills;
        }

        for batch in side.batches().batches() {
            let cells: Vec<&Cell> = batch
                .cells
                .iter()
                .filter_map(|index| side.grid().cell(*index))
                .collect();
            let core = planned_fill(
                planned,
                Unit::Batch(direction, batch.index),
                OrderPurpose::Core,
                market_price,
            );
            match core {
                Some(core) => {
                    fills.push(core);
                    // 主仓买完，本批格子全部启用：已经启用的按第一步的单，没启用的按建仓价。
                    for cell in cells {
                        if enabled(cell) {
                            fills.extend(planned_cell(cell));
                        } else if let Some(fill) = cell_entry_fill(direction, cell, market_price) {
                            fills.push(fill);
                        }
                    }
                }
                // 主仓这一步不立即成交（已经结束、还没放行、价格没到、避让后不够最低金额）：
                // 只有已经启用的格子在场上。后面的批次照样逐批看——这一批不挂单不说明后面的
                // 也不挂：避让只压低靠近自身卖单的那一批，更远的批次照原价挂出。
                None => {
                    for cell in cells {
                        if enabled(cell) {
                            fills.extend(planned_cell(cell));
                        }
                    }
                }
            }
        }
        fills
    }
}

/// 第一步规划出的这张单会不会立即成交：买单限价不低于当前价，卖单限价不高于当前价。
fn planned_fill(
    planned: &[OrderIntent],
    unit: Unit,
    purpose: OrderPurpose,
    market_price: Price,
) -> Option<ImmediateFill> {
    let intent = planned
        .iter()
        .find(|intent| intent.unit == unit && intent.purpose == purpose)?;
    let crosses = match intent.side {
        Side::Buy => market_price <= intent.limit_price,
        Side::Sell => market_price >= intent.limit_price,
    };
    crosses.then_some(ImmediateFill {
        unit,
        purpose,
        limit_price: intent.limit_price,
        quantity: intent.quantity,
    })
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

/// 一格从空仓开始时会不会立即建仓：它的建仓价已经到达当前价。
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

/// 账本上的持仓成本（第 7.6 节）：主仓已成交的部分按批次价、格子持有的部分按格子建仓价，
/// 多头记正、空头记负。
///
/// 只算现在还拿着的仓位：卖掉的格子不再摊进均价，交易所的持仓均价做不到这一点。和网格
/// 已实现利润同一口径，按计划价算，不含持有部分的建仓手续费，所以是估算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoldingCost {
    /// 账本上的净持仓。
    pub position: Position,
    /// 带符号的建仓金额：多头买入花的钱记正，空头开空收的钱记负。
    pub cost: Notional,
}

impl HoldingCost {
    /// 按一份份持仓（哪一侧、多少、建仓价）加起来。
    pub fn of(lots: impl IntoIterator<Item = (Direction, Quantity, Price)>) -> Self {
        lots.into_iter()
            .fold(Self::default(), |total, (direction, quantity, price)| {
                let amount = Notional::of(quantity, price);
                match direction {
                    Direction::Long => Self {
                        position: Position::from_units(total.position.units() + quantity.units()),
                        cost: total.cost + amount,
                    },
                    Direction::Short => Self {
                        position: Position::from_units(total.position.units() - quantity.units()),
                        cost: total.cost - amount,
                    },
                }
            })
    }

    /// 批次表和格子表里还拿着的部分：主仓累计成交，格子持有量。
    pub fn of_tables(batches: &[BatchSummary], cells: &[CellSummary]) -> Self {
        Self::of(
            batches
                .iter()
                .map(|batch| (batch.direction, batch.core_filled, batch.price))
                .chain(
                    cells
                        .iter()
                        .map(|cell| (cell.direction, cell.held, cell.entry_price)),
                ),
        )
    }

    /// 持仓均价；没有净持仓时是 `None`。
    pub fn average_price(&self) -> Option<Price> {
        self.price_of(self.cost)
    }

    /// 按 `price` 算的浮动盈亏：`净持仓 × 价格 − 建仓金额`。
    pub fn unrealized(&self, price: Price) -> Notional {
        signed_notional(self.position, price) - self.cost
    }

    /// 保本价：价格到这里时浮动盈亏正好抵掉 `realized`（已实现的盈亏，含资金费），也就是
    /// `均价 − realized ÷ 净持仓`。没有净持仓、或者算出来不是正数时是 `None`。
    pub fn breakeven_price(&self, realized: Notional) -> Option<Price> {
        self.price_of(self.cost - realized)
    }

    /// `amount ÷ 净持仓`，四舍五入到最小报价单位；不是正数时是 `None`。
    fn price_of(&self, amount: Notional) -> Option<Price> {
        let position = i128::from(self.position.units());
        if position == 0 {
            return None;
        }
        // 金额是 1e-16、仓位是 1e-8，相除正好是 1e-8 的价格。
        let (numerator, denominator) = if position < 0 {
            (-amount.raw(), -position)
        } else {
            (amount.raw(), position)
        };
        let units = (2 * numerator + denominator).div_euclid(2 * denominator);
        (units > 0)
            .then(|| i64::try_from(units).ok())
            .flatten()
            .map(Price::from_units)
    }
}

/// 带符号的仓位 × 价格。
fn signed_notional(position: Position, price: Price) -> Notional {
    let amount = Notional::of(Quantity::from_units(position.units().abs()), price);
    if position.units() < 0 {
        Notional::ZERO - amount
    } else {
        amount
    }
}

#[cfg(test)]
mod tests {
    use super::{CellStatus, HoldingCost, ImmediateFill};
    use crate::Strategy;
    use crate::batch::BatchIndex;
    use crate::config::SideConfig;
    use crate::fixtures::{
        btc_fees, cheap_spec, core_long_strategy, core_short_strategy, long_grid_strategy,
        neutral_grid_config, neutral_grid_strategy, single_small_batch_strategy,
        throttled_core_long_strategy, throttled_long_grid_strategy,
    };
    use crate::grid::CellIndex;
    use crate::order::{ClientOrderId, OrderPurpose, Unit, WorkingOrder};
    use crate::side::Direction;
    use crate::state::StrategyState;
    use crate::units::{Notional, Position, Price, Quantity};
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

    fn long_batch(index: usize) -> Unit {
        Unit::Batch(Direction::Long, BatchIndex(index))
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

        let batches = strategy.batch_summaries(&strategy.initial_state());

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
        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("90000").unwrap());

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
        assert_eq!(preview, expected);
    }

    #[test]
    fn nothing_fills_immediately_when_the_price_is_above_the_range() {
        let strategy = core_long_strategy();

        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("96000").unwrap());

        assert!(preview.is_empty());
    }

    #[test]
    fn every_batch_fills_immediately_when_the_price_is_at_the_bottom() {
        let strategy = core_long_strategy();

        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("75000").unwrap());

        // 四批主仓加全部 20 格。
        assert_eq!(preview.len(), 24);
        assert_eq!(
            preview.last().unwrap(),
            &fill(long_cell(19), "76000", "0.015")
        );
    }

    /// 接管 .37 之后价格在 88（第 8 节例子）：95 批已经买完，不再买；90 批只买还差的
    /// 主仓 .091，按避让后的 88999 挂出；88 格补回待买回的 .009。持有的格子平仓价都在
    /// 上方，不会立即卖出。
    #[test]
    fn b21_a_takeover_preview_lists_only_what_is_still_missing() {
        let strategy = core_long_strategy();
        let state = strategy
            .takeover_state(crate::units::Position::parse("0.37").unwrap())
            .unwrap();

        let preview = strategy.immediate_fills(&state, Price::parse("88000").unwrap());

        assert_eq!(
            preview,
            vec![
                ImmediateFill {
                    unit: long_batch(1),
                    purpose: OrderPurpose::Core,
                    // 第一步实际挂出的价格：避开自身最低卖价 89000 下方一个报价单位。
                    limit_price: Price::parse("88999").unwrap(),
                    quantity: Quantity::parse("0.091").unwrap(),
                },
                fill(long_cell(7), "88000", "0.009"),
            ]
        );

        // 价格在 89000：原价 90000 会成交，可实际挂出的 88999 不会。
        let preview = strategy.immediate_fills(&state, Price::parse("89000").unwrap());
        assert!(
            preview
                .iter()
                .all(|fill| fill.purpose != OrderPurpose::Core),
            "{preview:?}"
        );
    }

    /// 一批主仓避让后不够最低金额、这一步不挂：后面的批次照原价挂出，到价一样立即成交。
    ///
    /// 数量精度 .00001，接管 .74982：前两批完整；第三批主仓记 .17488、还差 .00012，按
    /// 85000 有 10.2 美元，本批不结束；可避开自身最低卖价 82000 后挂在 81999，只有 9.84
    /// 美元，这一步不挂。第四批 .175 照常挂在 80000。
    #[test]
    fn a_later_batch_still_fills_when_an_earlier_core_order_is_withheld() {
        let strategy = crate::fixtures::fine_core_long_strategy();
        let state = strategy
            .takeover_state(crate::units::Position::parse("0.74982").unwrap())
            .unwrap();

        assert!(
            strategy
                .immediate_fills(&state, Price::parse("80500").unwrap())
                .iter()
                .all(|fill| fill.purpose != OrderPurpose::Core)
        );
        let preview = strategy.immediate_fills(&state, Price::parse("79999").unwrap());
        assert!(
            preview.contains(&ImmediateFill {
                unit: long_batch(3),
                purpose: OrderPurpose::Core,
                limit_price: Price::parse("80000").unwrap(),
                quantity: Quantity::parse("0.175").unwrap(),
            }),
            "{preview:?}"
        );
        assert!(preview.iter().all(|fill| fill.unit != long_batch(2)));
    }

    /// 接管后价格已经涨过持有格子的平仓价：这些平仓单一挂上就会成交。
    #[test]
    fn a_takeover_preview_lists_exits_that_are_already_reached() {
        let strategy = core_long_strategy();
        let state = strategy
            .takeover_state(crate::units::Position::parse("0.37").unwrap())
            .unwrap();

        let preview = strategy.immediate_fills(&state, Price::parse("95500").unwrap());

        let exits: Vec<&ImmediateFill> = preview
            .iter()
            .filter(|fill| fill.purpose == OrderPurpose::GridExit)
            .collect();
        // 持有的是 95 到 88 八格，平仓价 96 到 89：除了 95 格（96），其余七格都不高于 95.5。
        assert_eq!(exits.len(), 7);
        assert!(
            exits
                .iter()
                .all(|fill| fill.limit_price <= Price::parse("95500").unwrap())
        );
    }

    #[test]
    fn a_throttled_start_lists_only_the_units_it_releases() {
        let strategy = throttled_core_long_strategy();

        // 价格在全部批次价之下：没有限速时四批主仓会一起成交。
        assert_eq!(
            core_long_strategy()
                .immediate_fills(
                    &core_long_strategy().initial_state(),
                    Price::parse("75000").unwrap()
                )
                .len(),
            24
        );

        // 开启限速：启动只放行 95 批，其余三批不会挂单，也就不会立即成交。
        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("75000").unwrap());
        assert_eq!(
            preview,
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

        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("92000").unwrap());

        // `r = 0`：格子就是单位，启动只放行最高的那一格。
        assert_eq!(preview, vec![fill(long_cell(0), "95000", "0.05")]);
    }

    #[test]
    fn a_grid_without_core_lists_the_cells_that_fill_immediately() {
        let strategy = long_grid_strategy();

        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("92000").unwrap());

        assert!(
            strategy
                .batch_summaries(&strategy.initial_state())
                .is_empty()
        );
        let expected = vec![
            fill(long_cell(0), "95000", "0.05"),
            fill(long_cell(1), "94000", "0.05"),
            fill(long_cell(2), "93000", "0.05"),
            fill(long_cell(3), "92000", "0.05"),
        ];
        assert_eq!(preview, expected);
    }

    /// 空头侧是多头侧的镜像：价格越低越到不了，所以启动时立即成交的是低处的批次和格子。
    #[test]
    fn b10_the_preview_mirrors_the_long_side_on_the_short_side() {
        let strategy = core_short_strategy();

        // 价格 76000：75000 那一批的主仓立即开空，跟随它的 75、76 两格也立即开空；
        // 80 及以上的批次价格还没到。
        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("76000").unwrap());

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
        assert_eq!(preview, expected);
        // 批次表也是镜像：批次价从 `L` 往上，主仓与网格数量跟多头侧一样。
        let batches = strategy.batch_summaries(&strategy.initial_state());
        assert_eq!(batches.len(), 4);
        assert!(
            batches
                .iter()
                .all(|batch| batch.direction == Direction::Short)
        );
        let prices: Vec<Price> = batches.iter().map(|batch| batch.price).collect();
        assert_eq!(
            prices,
            ["75000", "80000", "85000", "90000"]
                .map(|price| Price::parse(price).unwrap())
                .to_vec()
        );

        // 价格在范围下方时一张也不会立即成交：空头侧的开空价都还没到。
        assert!(
            strategy
                .immediate_fills(&strategy.initial_state(), Price::parse("74000").unwrap())
                .is_empty()
        );

        // 价格越过整个范围时反过来：四批主仓和二十格全部立即开空。
        let all =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("96000").unwrap());
        assert_eq!(all.len(), 24);
        assert_eq!(all.last().unwrap(), &fill(short_cell(19), "94000", "0.015"));
    }

    /// 中性网格（第 13 节 B12）：价格 93 已经越过空头侧的三格，多头侧一格都到不了。
    #[test]
    fn b12_the_preview_shows_the_short_cells_that_open_immediately() {
        let strategy = neutral_grid_strategy();

        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("93").unwrap());

        // 空头侧 91–96 五格各 .05，91、92、93 三格立即开空；多头侧最高买价 89，够不着。
        assert_eq!(
            preview,
            vec![
                fill(short_cell(0), "91", "0.05"),
                fill(short_cell(1), "92", "0.05"),
                fill(short_cell(2), "93", "0.05"),
            ]
        );
        // 两侧都没有主仓，批次表是空的。
        assert!(
            strategy
                .batch_summaries(&strategy.initial_state())
                .is_empty()
        );
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

        let preview =
            strategy.immediate_fills(&strategy.initial_state(), Price::parse("93").unwrap());

        // 空头侧只放行 91 那一格；92、93 还没放行，这一步根本不挂单。
        assert_eq!(preview, vec![fill(short_cell(0), "91", "0.05")]);
        // 多头侧不限速，十格都放行，但价格 93 够不着它们的买价。
        assert!(
            strategy
                .batch_summaries(&strategy.initial_state())
                .is_empty()
        );
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

    /// 第 11 节的格子表：买价、卖价、数量、持有、待买回、在场订单和状态。
    #[test]
    fn the_cell_table_shows_prices_quantities_and_status() {
        let strategy = core_long_strategy();
        let state = strategy.initial_state();

        let cells = strategy.cell_summaries(&state, &[]);

        assert_eq!(cells.len(), 20);
        let first = &cells[0];
        assert_eq!(first.direction, Direction::Long);
        assert_eq!(first.index, CellIndex(0));
        assert_eq!(first.batch, Some(BatchIndex(0)));
        assert_eq!(first.entry_price, Price::parse("95000").unwrap());
        assert_eq!(first.exit_price, Price::parse("96000").unwrap());
        assert_eq!(first.quantity, Quantity::parse("0.015").unwrap());
        assert_eq!(first.held, Quantity::ZERO);
        assert_eq!(first.to_buy_back, Quantity::ZERO);
        assert_eq!(first.working_orders, 0);
        assert_eq!(first.status, CellStatus::WaitingForBatch);
    }

    /// 状态跟着格子状态走：启用后等买入，买回后持有，卖出一部分后待买回。
    #[test]
    fn the_cell_table_follows_the_cell_through_its_cycle() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();
        // 第 0 批主仓买完：本批五格启用。
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.175").unwrap();
        strategy.advance(&mut state, &[]);

        let status =
            |state: &crate::state::StrategyState| strategy.cell_summaries(state, &[])[0].status;
        assert_eq!(status(&state), CellStatus::WaitingForBuy);

        // 整格买回：持有整格。
        long_state_mut(&mut state)
            .cell_mut(CellIndex(0))
            .unwrap()
            .to_buy_back = Quantity::ZERO;
        let cell = &strategy.cell_summaries(&state, &[])[0];
        assert_eq!(cell.status, CellStatus::Holding);
        assert_eq!(cell.held, Quantity::parse("0.015").unwrap());

        // 卖出 .005：持有 .010，待买回 .005。
        long_state_mut(&mut state)
            .cell_mut(CellIndex(0))
            .unwrap()
            .to_buy_back = Quantity::parse("0.005").unwrap();
        let cell = &strategy.cell_summaries(&state, &[])[0];
        assert_eq!(cell.status, CellStatus::ToBuyBack);
        assert_eq!(cell.held, Quantity::parse("0.010").unwrap());
        assert_eq!(cell.to_buy_back, Quantity::parse("0.005").unwrap());
    }

    /// 在场订单按格子统计。
    #[test]
    fn the_cell_table_counts_the_working_orders_of_each_cell() {
        let strategy = long_grid_strategy();
        let state = strategy.initial_state();
        let working = vec![
            working_order(
                Unit::Cell(Direction::Long, CellIndex(0)),
                OrderPurpose::GridEntry,
            ),
            working_order(
                Unit::Cell(Direction::Long, CellIndex(3)),
                OrderPurpose::GridExit,
            ),
        ];

        let cells = strategy.cell_summaries(&state, &working);

        assert_eq!(cells[0].working_orders, 1);
        assert_eq!(cells[1].working_orders, 0);
        assert_eq!(cells[3].working_orders, 1);
    }

    /// 每格扣费后收益：双边都按 taker 成交也要为正，maker 成交时更高（第 2.3 节）。
    #[test]
    fn the_cell_table_shows_the_profit_after_fees_on_both_legs() {
        let strategy = long_grid_strategy();
        let state = strategy.initial_state();

        let cell = &strategy.cell_summaries(&state, &[])[0];

        // 95000 买、96000 卖、.05 个：毛收益 50。
        // taker 双边费率 .00045：手续费 (95000 + 96000) × .05 × .00045 = 4.2975。
        assert_eq!(cell.profit_taker, Notional::parse("45.7025").unwrap());
        // maker 双边费率 .00015：手续费 1.4325。
        assert_eq!(cell.profit_maker, Notional::parse("48.5675").unwrap());
        assert!(cell.profit_taker < cell.profit_maker);
    }

    fn working_order(unit: Unit, purpose: OrderPurpose) -> WorkingOrder {
        WorkingOrder {
            client_order_id: ClientOrderId::try_new("0x01").unwrap(),
            unit,
            purpose,
            limit_price: Price::parse("95000").unwrap(),
            remaining: Quantity::parse("0.05").unwrap(),
        }
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

        assert_eq!(batches[0].direction, Direction::Long);
        assert_eq!(batches[0].core_filled, Quantity::parse("0.07").unwrap());
        assert_eq!(batches[0].enabled_cell_count, 2);
        assert_eq!(batches[0].cell_count, 5);
        assert_eq!(batches[1].enabled_cell_count, 0);
    }

    /// 网格已实现利润：平仓腿成交了整格，就是这一格的 maker 扣费后收益；两格相加；主仓
    /// 和整侧的单位不算。
    #[test]
    fn the_realized_grid_profit_adds_up_the_closed_cells() {
        let strategy = long_grid_strategy();
        let cells = strategy.cell_summaries(&strategy.initial_state(), &[]);
        let exits = [
            (Unit::Cell(Direction::Long, CellIndex(0)), cells[0].quantity),
            (Unit::Cell(Direction::Long, CellIndex(1)), cells[1].quantity),
            (
                Unit::Batch(Direction::Long, BatchIndex(0)),
                Quantity::parse("1").unwrap(),
            ),
            (Unit::Side(Direction::Long), Quantity::parse("1").unwrap()),
        ];

        assert_eq!(
            strategy.grid_realized_profit(&exits),
            cells[0].profit_maker + cells[1].profit_maker
        );
        assert_eq!(strategy.grid_realized_profit(&[]), Notional::ZERO);
    }

    fn lot(direction: Direction, quantity: &str, price: &str) -> (Direction, Quantity, Price) {
        (
            direction,
            Quantity::parse(quantity).unwrap(),
            Price::parse(price).unwrap(),
        )
    }

    /// 主仓 1 在 100、格子 1 在 99：均价 99.5；标记价 98 时浮亏 3；网格已赚 1 时保本价 99。
    #[test]
    fn the_holding_cost_averages_what_is_still_held() {
        let cost = HoldingCost::of([
            lot(Direction::Long, "1", "100"),
            lot(Direction::Long, "1", "99"),
        ]);

        assert_eq!(cost.position, Position::parse("2").unwrap());
        assert_eq!(cost.average_price(), Some(Price::parse("99.5").unwrap()));
        assert_eq!(
            cost.unrealized(Price::parse("98").unwrap()),
            Notional::ZERO - Notional::parse("3").unwrap()
        );
        assert_eq!(
            cost.breakeven_price(Notional::parse("1").unwrap()),
            Some(Price::parse("99").unwrap())
        );
    }

    /// 空头：价格跌了是浮盈，已实现的盈利让保本价往上走。
    #[test]
    fn a_short_holding_gains_when_the_price_falls() {
        let cost = HoldingCost::of([lot(Direction::Short, "2", "100")]);

        assert_eq!(cost.position, Position::parse("-2").unwrap());
        assert_eq!(cost.average_price(), Some(Price::parse("100").unwrap()));
        assert_eq!(
            cost.unrealized(Price::parse("98").unwrap()),
            Notional::parse("4").unwrap()
        );
        assert_eq!(
            cost.breakeven_price(Notional::parse("2").unwrap()),
            Some(Price::parse("101").unwrap())
        );
    }

    /// 没有净持仓就没有均价和保本价；已实现的盈利超过持仓成本时保本价不是正数，也不给。
    #[test]
    fn no_holding_means_no_average_or_breakeven() {
        let empty = HoldingCost::of([]);
        assert_eq!(empty.average_price(), None);
        assert_eq!(empty.breakeven_price(Notional::ZERO), None);
        assert_eq!(
            empty.unrealized(Price::parse("98").unwrap()),
            Notional::ZERO
        );

        let small = HoldingCost::of([lot(Direction::Long, "0.01", "100")]);
        assert_eq!(small.breakeven_price(Notional::parse("5").unwrap()), None);
    }
}
