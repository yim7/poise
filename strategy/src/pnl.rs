//! 盈亏（第 7.6 节）：按实际成交，把每批主仓、每个格子的建仓和平仓先进先出配对，算出
//! 网格已实现利润、减仓已实现，以及还拿着的仓位的成本。
//!
//! 不用交易所的持仓均价：它把后来被网格卖掉的买入也摊进均价，网格卖出时均价又不变，和
//! 策略手上这些仓位的成本对不上。

use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Utc};

use crate::order::{OrderPurpose, Unit};
use crate::preview::{BatchSummary, CellSummary};
use crate::side::Direction;
use crate::units::{Notional, Position, Price, Quantity};

/// 一笔成交：属于哪个单位的哪条腿、成交价、数量、手续费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    pub unit: Unit,
    pub purpose: OrderPurpose,
    pub price: Price,
    pub quantity: Quantity,
    /// 付的手续费（负数是返佣）。
    pub fee: Notional,
    pub at: DateTime<Utc>,
}

/// 账本上一个单位持有多少，和它的计划建仓价（接管来的那部分没有成交，按它算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holding {
    pub unit: Unit,
    pub quantity: Quantity,
    pub planned_price: Price,
}

/// 批次表和格子表里还拿着的部分：主仓累计成交按批次价，格子持有量按格子建仓价。
pub fn holdings(batches: &[BatchSummary], cells: &[CellSummary]) -> Vec<Holding> {
    batches
        .iter()
        .map(|batch| Holding {
            unit: Unit::Batch(batch.direction, batch.index),
            quantity: batch.core_filled,
            planned_price: batch.price,
        })
        .chain(cells.iter().map(|cell| Holding {
            unit: Unit::Cell(cell.direction, cell.index),
            quantity: cell.held,
            planned_price: cell.entry_price,
        }))
        .collect()
}

/// 算出来的盈亏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfitAndLoss {
    /// 网格已实现利润：格子平仓腿卖出的钱，减去配对的建仓成本和两边手续费。
    pub grid_realized: Notional,
    /// 失效减仓和市价平仓已经实现的盈亏；还没有减仓成交时是 `None`。
    pub reduction_realized: Option<Notional>,
    /// 还拿着的仓位的成本。
    pub holding: HoldingCost,
}

/// 按实际成交算盈亏。
///
/// `before` 是减仓之前的账本，`after` 是扣掉失效减仓和市价平仓之后的账本（批次表、格子表
/// 显示的那份）。`executions` 是这个策略全部订单的成交，含减仓单。
///
/// - 每个单位按时间先后配对：建仓腿成交进队尾，平仓腿成交从队头取。接管来的持有量没有
///   成交，按计划建仓价放在队头（它最早）。
/// - 减仓按账本从每个单位扣掉的数量，从队头取；减仓单的成交按发出先后归给这些数量，
///   超出账本的部分（平仓连带平掉的账本外仓位）不是本策略的，不算。
/// - 成交记录和账本对不上时（缺了流水），以账本为准：多出来的从队头扔掉，少的按计划价补上。
pub fn profit_and_loss(
    before: &[Holding],
    after: &[Holding],
    executions: &[Execution],
) -> ProfitAndLoss {
    let mut executions: Vec<&Execution> = executions.iter().collect();
    executions.sort_by_key(|execution| execution.at);

    let mut lots: BTreeMap<Unit, VecDeque<Lot>> = BTreeMap::new();
    for holding in before {
        let (entered, exited) = executions
            .iter()
            .filter(|execution| execution.unit == holding.unit)
            .fold(
                (0_i64, 0_i64),
                |(entered, exited), execution| match execution.purpose {
                    OrderPurpose::Core | OrderPurpose::GridEntry => {
                        (entered + execution.quantity.units(), exited)
                    }
                    OrderPurpose::GridExit => (entered, exited + execution.quantity.units()),
                    _ => (entered, exited),
                },
            );
        let taken_over = holding.quantity.units() - (entered - exited);
        let queue = lots.entry(holding.unit).or_default();
        if taken_over > 0 {
            queue.push_back(Lot::planned(taken_over, holding.planned_price));
        }
    }

    let planned = |unit: Unit, fallback: Price| {
        before
            .iter()
            .find(|holding| holding.unit == unit)
            .map_or(fallback, |holding| holding.planned_price)
    };

    let mut grid_realized = Notional::ZERO;
    for execution in &executions {
        match execution.purpose {
            OrderPurpose::Core | OrderPurpose::GridEntry => {
                lots.entry(execution.unit).or_default().push_back(Lot {
                    quantity: execution.quantity.units(),
                    amount: Notional::of(execution.quantity, execution.price),
                    fee: execution.fee,
                });
            }
            OrderPurpose::GridExit => {
                let queue = lots.entry(execution.unit).or_default();
                let price = planned(execution.unit, execution.price);
                let taken = take(queue, execution.quantity.units(), price);
                let proceeds = Notional::of(execution.quantity, execution.price);
                grid_realized = grid_realized
                    + signed(execution.unit.direction(), proceeds - taken.amount)
                    - execution.fee
                    - taken.fee;
            }
            OrderPurpose::Invalidation | OrderPurpose::Close => {}
        }
    }
    for holding in before {
        let queue = lots.entry(holding.unit).or_default();
        settle(queue, holding.quantity.units(), holding.planned_price);
    }

    // 减仓：从每个单位扣掉账本减掉的数量，按侧汇总成本。
    let mut reduced: BTreeMap<Direction, (i64, Lot)> = BTreeMap::new();
    for holding in before {
        let left = after
            .iter()
            .find(|after| after.unit == holding.unit)
            .map_or(0, |after| after.quantity.units());
        let deducted = (holding.quantity.units() - left).max(0);
        let queue = lots.entry(holding.unit).or_default();
        let taken = take(queue, deducted, holding.planned_price);
        let side = reduced
            .entry(holding.unit.direction())
            .or_insert((0, Lot::default()));
        side.0 += deducted;
        side.1 = side.1.plus(taken);
    }
    let reductions: Vec<&&Execution> = executions
        .iter()
        .filter(|execution| execution.purpose.is_reduction())
        .collect();
    let reduction_realized = (!reductions.is_empty()).then(|| {
        reduced
            .iter()
            .map(|(&direction, &(deducted, cost))| {
                let mut left = deducted;
                let mut proceeds = Notional::ZERO;
                let mut fee = Notional::ZERO;
                for execution in reductions
                    .iter()
                    .filter(|execution| execution.unit.direction() == direction)
                {
                    let quantity = execution.quantity.units();
                    let taken = quantity.min(left);
                    left -= taken;
                    proceeds = proceeds
                        + Notional::of(execution.quantity, execution.price).scale(taken, quantity);
                    fee = fee + execution.fee.scale(taken, quantity);
                }
                signed(direction, proceeds - cost.amount) - fee - cost.fee
            })
            .fold(Notional::ZERO, |total, realized| total + realized)
    });

    let mut holding = HoldingCost::default();
    for after in after {
        let queue = lots.entry(after.unit).or_default();
        settle(queue, after.quantity.units(), after.planned_price);
        let direction = after.unit.direction();
        for lot in queue.iter() {
            holding = holding.plus(direction, lot);
        }
    }

    ProfitAndLoss {
        grid_realized,
        reduction_realized,
        holding,
    }
}

/// 一份还拿着的仓位：数量（1e-8）、成交金额和建仓手续费。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Lot {
    quantity: i64,
    amount: Notional,
    fee: Notional,
}

impl Lot {
    /// 按计划价、不带手续费的一份：接管来的，或者流水缺了按计划价补上的。
    fn planned(quantity: i64, price: Price) -> Self {
        Self {
            quantity,
            amount: Notional::of(Quantity::from_units(quantity), price),
            fee: Notional::ZERO,
        }
    }

    fn plus(self, other: Lot) -> Self {
        Self {
            quantity: self.quantity + other.quantity,
            amount: self.amount + other.amount,
            fee: self.fee + other.fee,
        }
    }
}

/// 从队头取 `quantity`，不够的部分按 `price` 补（流水缺了）。
fn take(queue: &mut VecDeque<Lot>, quantity: i64, price: Price) -> Lot {
    let mut taken = Lot::default();
    let mut left = quantity;
    while left > 0 {
        let Some(lot) = queue.front_mut() else {
            return taken.plus(Lot::planned(left, price));
        };
        if lot.quantity <= left {
            left -= lot.quantity;
            taken = taken.plus(*lot);
            queue.pop_front();
        } else {
            let part = Lot {
                quantity: left,
                amount: lot.amount.scale(left, lot.quantity),
                fee: lot.fee.scale(left, lot.quantity),
            };
            *lot = Lot {
                quantity: lot.quantity - left,
                amount: lot.amount - part.amount,
                fee: lot.fee - part.fee,
            };
            taken = taken.plus(part);
            left = 0;
        }
    }
    taken
}

/// 让队里正好剩 `quantity`：多了从队头扔掉，少了按 `price` 在队头补上。
fn settle(queue: &mut VecDeque<Lot>, quantity: i64, price: Price) {
    let held: i64 = queue.iter().map(|lot| lot.quantity).sum();
    if held > quantity {
        take(queue, held - quantity, price);
    } else if held < quantity {
        queue.push_front(Lot::planned(quantity - held, price));
    }
}

/// 多头原样，空头取反：多头卖出收钱是赚，空头买回花钱是亏。
fn signed(direction: Direction, amount: Notional) -> Notional {
    match direction {
        Direction::Long => amount,
        Direction::Short => Notional::ZERO - amount,
    }
}

/// 还拿着的仓位的成本，多头记正、空头记负。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoldingCost {
    /// 净持仓。
    pub position: Position,
    /// 带符号的建仓金额：多头买入花的钱记正，空头开空收的钱记负。
    pub cost: Notional,
    /// 这些仓位建仓时付的手续费。
    pub fees: Notional,
}

impl HoldingCost {
    /// 按一份份持仓（哪一侧、多少、建仓价）加起来，不带手续费。
    pub fn of(lots: impl IntoIterator<Item = (Direction, Quantity, Price)>) -> Self {
        lots.into_iter()
            .fold(Self::default(), |total, (direction, quantity, price)| {
                total.plus(direction, &Lot::planned(quantity.units(), price))
            })
    }

    fn plus(self, direction: Direction, lot: &Lot) -> Self {
        let quantity = match direction {
            Direction::Long => lot.quantity,
            Direction::Short => -lot.quantity,
        };
        Self {
            position: Position::from_units(self.position.units() + quantity),
            cost: self.cost + signed(direction, lot.amount),
            fees: self.fees + lot.fee,
        }
    }

    /// 持仓均价（不含手续费）；没有净持仓、或者算出来不是正数时是 `None`。
    pub fn average_price(&self) -> Option<Price> {
        self.price_of(self.cost)
    }

    /// 按 `price` 算的浮动盈亏：`净持仓 × 价格 − 建仓金额 − 建仓手续费`。
    pub fn unrealized(&self, price: Price) -> Notional {
        let value = signed_notional(self.position, price);
        value - self.cost - self.fees
    }

    /// 保本价：价格到这里时浮动盈亏正好抵掉 `realized`（已实现的盈亏，含资金费）。没有净持仓、
    /// 或者算出来不是正数时是 `None`。
    pub fn breakeven_price(&self, realized: Notional) -> Option<Price> {
        self.price_of(self.cost + self.fees - realized)
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
    use super::{Execution, Holding, HoldingCost, profit_and_loss};
    use crate::batch::BatchIndex;
    use crate::grid::CellIndex;
    use crate::order::{OrderPurpose, Unit};
    use crate::side::Direction;
    use crate::units::{Notional, Position, Price, Quantity};
    use chrono::{DateTime, Utc};

    fn price(value: &str) -> Price {
        Price::parse(value).unwrap()
    }

    fn quantity(value: &str) -> Quantity {
        Quantity::parse(value).unwrap()
    }

    fn notional(value: &str) -> Notional {
        Notional::parse(value).unwrap()
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_792_000_000 + seconds, 0).unwrap()
    }

    const CELL: Unit = Unit::Cell(Direction::Long, CellIndex(0));
    const CORE: Unit = Unit::Batch(Direction::Long, BatchIndex(0));
    const SHORT_CELL: Unit = Unit::Cell(Direction::Short, CellIndex(0));

    fn execution(
        unit: Unit,
        purpose: OrderPurpose,
        px: &str,
        qty: &str,
        fee: &str,
        second: i64,
    ) -> Execution {
        Execution {
            unit,
            purpose,
            price: price(px),
            quantity: quantity(qty),
            fee: notional(fee),
            at: at(second),
        }
    }

    fn holding(unit: Unit, qty: &str, planned: &str) -> Holding {
        Holding {
            unit,
            quantity: quantity(qty),
            planned_price: price(planned),
        }
    }

    /// 计划 100 买、101 卖的格子：实际 99.5 买进、101 卖出，又 99.8 买回。网格利润按实际
    /// 价差和手续费算；还拿着的那份成本是 99.8 那笔，不是计划价 100。
    #[test]
    fn a_cell_pairs_its_actual_buys_and_sells_first_in_first_out() {
        let executions = [
            execution(CELL, OrderPurpose::GridEntry, "99.5", "1", "0.01", 1),
            execution(CELL, OrderPurpose::GridExit, "101", "1", "0.02", 2),
            execution(CELL, OrderPurpose::GridEntry, "99.8", "1", "0.03", 3),
        ];
        let ledger = [holding(CELL, "1", "100")];

        let pnl = profit_and_loss(&ledger, &ledger, &executions);

        assert_eq!(pnl.grid_realized, notional("1.47"));
        assert_eq!(pnl.reduction_realized, None);
        assert_eq!(pnl.holding.position, Position::parse("1").unwrap());
        assert_eq!(pnl.holding.average_price(), Some(price("99.8")));
        assert_eq!(pnl.holding.fees, notional("0.03"));
        // 99 时：1 × 99 − 99.8 − 0.03。
        assert_eq!(
            pnl.holding.unrealized(price("99")),
            Notional::ZERO - notional("0.83")
        );
        // 合计为零的价格：99.8 + 0.03 − (1.47 + 0)。
        assert_eq!(
            pnl.holding.breakeven_price(pnl.grid_realized),
            Some(price("98.36"))
        );
    }

    /// 接管来的持有量没有成交，按计划价放在最前：先卖掉的是它。
    #[test]
    fn a_taken_over_holding_is_sold_first_at_the_planned_price() {
        let executions = [
            execution(CELL, OrderPurpose::GridExit, "101", "1", "0", 1),
            execution(CELL, OrderPurpose::GridEntry, "99", "1", "0", 2),
        ];
        let ledger = [holding(CELL, "1", "100")];

        let pnl = profit_and_loss(&ledger, &ledger, &executions);

        assert_eq!(pnl.grid_realized, notional("1"));
        assert_eq!(pnl.holding.average_price(), Some(price("99")));
    }

    /// 空头格子：高处卖出开空，低处买回。
    #[test]
    fn a_short_cell_gains_when_it_buys_back_lower() {
        let executions = [
            execution(SHORT_CELL, OrderPurpose::GridEntry, "101", "2", "0", 1),
            execution(SHORT_CELL, OrderPurpose::GridExit, "100", "1", "0", 2),
        ];
        let ledger = [holding(SHORT_CELL, "1", "101")];

        let pnl = profit_and_loss(&ledger, &ledger, &executions);

        assert_eq!(pnl.grid_realized, notional("1"));
        assert_eq!(pnl.holding.position, Position::parse("-1").unwrap());
        assert_eq!(pnl.holding.average_price(), Some(price("101")));
        assert_eq!(pnl.holding.unrealized(price("99")), notional("2"));
    }

    /// 平仓连带平掉账本外的 1：只有账本上那 2 算本策略的，按成交先后归属。
    #[test]
    fn a_close_counts_only_what_the_ledger_held() {
        let executions = [
            execution(CORE, OrderPurpose::Core, "100", "1", "0", 1),
            execution(CELL, OrderPurpose::GridEntry, "99", "1", "0", 2),
            execution(
                Unit::Side(Direction::Long),
                OrderPurpose::Close,
                "95",
                "2",
                "0.2",
                3,
            ),
            execution(
                Unit::Side(Direction::Long),
                OrderPurpose::Close,
                "110",
                "1",
                "0.1",
                4,
            ),
        ];
        let before = [holding(CORE, "1", "100"), holding(CELL, "1", "99")];
        let after = [holding(CORE, "0", "100"), holding(CELL, "0", "99")];

        let pnl = profit_and_loss(&before, &after, &executions);

        // 2 × 95 − (100 + 99) − 0.2。
        assert_eq!(
            pnl.reduction_realized,
            Some(Notional::ZERO - notional("9.2"))
        );
        assert_eq!(pnl.holding, HoldingCost::default());
    }

    /// 成交流水缺了（服务停过很久）：以账本为准，缺的按计划价补上。
    #[test]
    fn missing_fills_fall_back_to_the_planned_price() {
        let ledger = [holding(CORE, "2", "100")];
        let executions = [execution(CORE, OrderPurpose::Core, "98", "1", "0", 1)];

        let pnl = profit_and_loss(&ledger, &ledger, &executions);

        assert_eq!(pnl.holding.position, Position::parse("2").unwrap());
        assert_eq!(pnl.holding.average_price(), Some(price("99")));
    }

    /// 没有净持仓就没有均价和保本价；已实现的盈利超过持仓成本时保本价不是正数，也不给。
    #[test]
    fn no_holding_means_no_average_or_breakeven() {
        let empty = HoldingCost::of([]);
        assert_eq!(empty.average_price(), None);
        assert_eq!(empty.breakeven_price(Notional::ZERO), None);
        assert_eq!(empty.unrealized(price("98")), Notional::ZERO);

        let small = HoldingCost::of([(Direction::Long, quantity("0.01"), price("100"))]);
        assert_eq!(small.breakeven_price(notional("5")), None);
    }
}
