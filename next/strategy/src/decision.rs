//! 判定：拿一份从交易所读回来的快照，说这一步能不能继续下单。
//!
//! 纯计算，不读时钟、不访问网络（第 12 节）。向交易所取数的 I/O 在 runtime 的读取
//! 阶段，这里只处理读回来的数据，所以可以直接用数据测试。
//!
//! 输入有三样：仓位核对的结论、成交流水与订单累计成交量是否对得上、本地订单快照。
//! 输出是一个 [`Decision`]：暂停原因、等待原因，以及哪些单位的哪些用途不能下单
//! （第 7.1、9.2 节）。
//!
//! 判定只回答「能不能下单」，不产生订单：要下什么单仍然由 [`crate::planning`] 决定。

use std::collections::HashMap;

use crate::order::{ClientOrderId, OrderPurpose, OrderRecord, Side, Unit};
use crate::planning::{PendingUnit, WaitReason};
use crate::units::{Position, Quantity};

/// 判定需要的一张订单的本地视图。
///
/// 只带判定用得到的字段：限价、数量、时间戳都不参与判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderState {
    pub client_order_id: ClientOrderId,
    pub unit: Unit,
    pub purpose: OrderPurpose,
    /// 本地已经记入的累计成交量。
    pub cumulative_filled: Quantity,
    /// 这张订单的成交是否已经对完账。
    pub fills_reconciled: bool,
}

impl OrderState {
    pub fn of(record: &OrderRecord, fills_reconciled: bool) -> Self {
        Self {
            client_order_id: record.client_order_id.clone(),
            unit: record.unit,
            purpose: record.purpose,
            cumulative_filled: record.cumulative_filled,
            fills_reconciled,
        }
    }
}

/// 这一步必须停下来时，策略停在哪一边。
///
/// 暂停是「继续自动重试没有意义」：撤销全部挂单，交回用户；等待只是「这一步的数据
/// 说明不了问题」，下一步再核对（第 9.2 节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    /// 仓位核对失败：连续两轮读到同一份数据，仍然解释不了差额。
    PositionCheckFailed,
    /// 成交流水与订单累计成交量对不上。
    FillRecordsInconsistent,
}

impl PauseReason {
    /// 记进 [`crate::planning::Plan::waiting`] 的原因。
    pub fn wait_reason(self) -> WaitReason {
        match self {
            Self::PositionCheckFailed => WaitReason::PositionCheckFailed,
            Self::FillRecordsInconsistent => WaitReason::FillRecordsInconsistent,
        }
    }
}

/// 这一步的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 可以继续规划并下单。
    Proceed,
    /// 这一步不下新单，下一步再核对。
    Wait(WaitReason),
    /// 暂停策略。
    Pause(PauseReason),
}

/// 一次判定的完整结果。
///
/// 除了「能不能下单」，还带上所有没对完账的单位与用途：撤单也会让订单失去对账，
/// 撤单之后要按这一份重新过滤规划出来的订单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    /// 还没有对完账、这一侧不能下新单的单位与用途。
    pub blocked: Vec<PendingUnit>,
}

/// 判定这一步能不能继续下单。
///
/// 仓位核对的问题优先于成交记录：数据还在变的时候，成交记录对不上可能只是流水还没到；
/// 仓位已经稳定且对不上时，再报成交记录的问题只会把注意力引偏。
pub fn judge(
    position: PositionCheck,
    inconsistent: Option<&ClientOrderId>,
    orders: &[OrderState],
) -> Decision {
    let verdict = match position {
        PositionCheck::Mismatch => Verdict::Pause(PauseReason::PositionCheckFailed),
        PositionCheck::Unstable => Verdict::Wait(WaitReason::PositionUnstable),
        PositionCheck::Consistent => match inconsistent {
            Some(_) => Verdict::Pause(PauseReason::FillRecordsInconsistent),
            None => Verdict::Proceed,
        },
    };
    Decision {
        verdict,
        blocked: pending_units(orders),
    }
}

/// 一轮仓位核对读到的数据：交易所的实际仓位、我们推算的应有仓位，以及每张订单
/// 交易所报的累计成交量快照。
///
/// 判断「数据是不是还在变」要整体比较这三项：只看净仓位会被一买一卖相抵骗过去
/// （第 7.1.3 节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionReading {
    pub actual: Position,
    pub expected: Position,
    pub reported: Vec<(ClientOrderId, Quantity)>,
}

/// 仓位核对的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionCheck {
    /// 实际仓位和应有仓位一致。
    Consistent,
    /// 数据还在变化：这一轮说明不了问题，下一步再核对。
    Unstable,
    /// 连续两轮读到同一份数据，仍然对不上：仓位异常。
    Mismatch,
}

/// 比对一轮读数与上一次读数。
///
/// `previous` 是上一轮读到的数据，只有读过两轮以上才有值。两次读到完全相同的数据
/// 还解释不了差额，就不是「数据在变」，而是仓位真的对不上。
pub fn position_check(
    reading: &PositionReading,
    previous: Option<&PositionReading>,
) -> PositionCheck {
    if reading.actual == reading.expected {
        PositionCheck::Consistent
    } else if previous == Some(reading) {
        PositionCheck::Mismatch
    } else {
        PositionCheck::Unstable
    }
}

/// 仓位核对时我们预计交易所会有的仓位。
///
/// 由格子状态算出的应有持仓，加上交易所已经成交、我们还没记入的数量：成交记录可能
/// 比挂单状态晚到，这段差额是能解释的（第 7.1.3 节）。
pub fn expected_position(
    base: Position,
    reported: &[(ClientOrderId, Quantity)],
    orders: &[OrderState],
) -> Position {
    let mut unrecorded = 0_i64;
    for (client_order_id, cumulative) in reported {
        let Some(order) = orders
            .iter()
            .find(|order| &order.client_order_id == client_order_id)
        else {
            continue;
        };
        // `reported` 是读取阶段的快照。补拉成交之后我们的记录可能已经追上甚至超过它，
        // 那时差额按 0 算，而不是负数。
        let unbooked = unbooked_quantity(*cumulative, order.cumulative_filled);
        unrecorded += signed_delta(order.unit.side(order.purpose), unbooked);
    }
    Position::from_units(base.units() + unrecorded)
}

/// 还没有对完账的单位：这些单位的这一侧只保留预留，不下新单。
///
/// 一条规则覆盖三种情况：撤单期间的部分成交、恢复时还没到的流水、超出回看窗口的迟到
/// 流水。它们的共同点是订单的成交还没对完账，这时按旧数量补单就会多买或多卖
/// （第 9.2 节）。
pub fn pending_units(orders: &[OrderState]) -> Vec<PendingUnit> {
    orders
        .iter()
        .filter(|order| !order.fills_reconciled)
        .map(|order| PendingUnit {
            unit: order.unit,
            purpose: order.purpose,
        })
        .collect()
}

/// 找出成交流水与订单累计成交量对不上的订单。
///
/// 判定标准是「按成交编号去重后的流水数量之和**超过**已记入的累计成交量」。
/// 正常数据不可能出现：每笔成交的累计量已经包含它之前的全部成交，所以一张订单全部
/// 流水之和最多等于它最终的累计成交量；超出说明流水里混进了不属于这张单的记录
/// （例如同一笔成交换了成交编号重复上报，或成交被归到了错误的订单上）。
///
/// 这种不一致不会自己消失：[`fills_settled`] 要求两边相等，多出来的部分永远对不平，
/// 订单会一直占着待处理状态、这一格再也挂不出新单。所以不能让它无声地卡住。
pub fn inconsistent_fill_records(
    saved_fills: &HashMap<ClientOrderId, Quantity>,
    orders: &[OrderState],
) -> Option<ClientOrderId> {
    orders.iter().find_map(|order| {
        let saved = saved_fills
            .get(&order.client_order_id)
            .copied()
            .unwrap_or(Quantity::ZERO);
        (saved > order.cumulative_filled).then(|| order.client_order_id.clone())
    })
}

/// 一张订单的成交是否已经对完账。
///
/// 对完账要同时满足两件事：
/// - 本地记入的累计成交量追上了交易所给的累计成交量（交易所的结局已知，见调用处）；
/// - 成交流水表里属于这张订单的数量之和，正好等于本地记入的累计成交量。
///
/// 流水之和按成交编号去重（`trade_id` 是流水表的主键，同一笔成交只会有一行）。
/// 用「等于」而不是「不少于」：重复报告同一笔成交不能填补另一笔真正缺失的成交。
/// 每笔成交各自带自己的数量，同一张订单的成交数量之和就等于它的累计成交量，
/// 所以到齐时一定相等，多出来的一定是重复或凭空多出的记录，不能算到齐。
///
/// 只要有一项不满足，这张订单就继续占用所属单位：规划时不为这个单位下同一用途的新单，
/// 补拉成交的回查起点也继续保留在它挂出去的时间。
pub fn fills_settled(
    recorded: Quantity,
    exchange_cumulative: Quantity,
    saved_fills: Quantity,
) -> bool {
    unbooked_quantity(exchange_cumulative, recorded).is_zero() && saved_fills == recorded
}

/// 交易所报的累计成交量减去我们已经记入的数量，即「成交了但还没记账」的部分。
///
/// 已经记入得不比它少时是 0。这里不能用 `Quantity::saturating_sub`：它是 i64 的
/// 饱和减法，减多了会给出负数，而「还差多少没记入」不可能为负。
pub fn unbooked_quantity(reported: Quantity, recorded: Quantity) -> Quantity {
    if reported > recorded {
        Quantity::from_units(reported.units() - recorded.units())
    } else {
        Quantity::ZERO
    }
}

/// 一笔成交对仓位的贡献，以 1e-8 标的为单位：买为正，卖为负。
pub fn signed_delta(side: Side, quantity: Quantity) -> i64 {
    match side {
        Side::Buy => quantity.units(),
        Side::Sell => -quantity.units(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OrderState, PauseReason, PositionCheck, PositionReading, Verdict, expected_position,
        fills_settled, inconsistent_fill_records, judge, pending_units, position_check,
        unbooked_quantity,
    };
    use crate::batch::BatchIndex;
    use crate::grid::CellIndex;
    use crate::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Unit};
    use crate::planning::{PendingUnit, WaitReason};
    use crate::side::Direction;
    use crate::units::{Position, Price, Quantity};
    use std::collections::HashMap;

    fn cloid(value: &str) -> ClientOrderId {
        ClientOrderId::try_new(value).unwrap()
    }

    fn order(
        id: &str,
        unit: Unit,
        purpose: OrderPurpose,
        cumulative_filled: &str,
        fills_reconciled: bool,
    ) -> OrderState {
        OrderState {
            client_order_id: cloid(id),
            unit,
            purpose,
            cumulative_filled: Quantity::parse(cumulative_filled).unwrap(),
            fills_reconciled,
        }
    }

    /// 第 0 格的一张买单，累计成交量按参数给定。
    fn cell_order(cumulative_filled: &str, fills_reconciled: bool) -> OrderState {
        order(
            "0x01",
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridEntry,
            cumulative_filled,
            fills_reconciled,
        )
    }

    fn reading(actual: &str, expected: &str) -> PositionReading {
        PositionReading {
            actual: Position::parse(actual).unwrap(),
            expected: Position::parse(expected).unwrap(),
            reported: Vec::new(),
        }
    }

    #[test]
    fn order_state_carries_the_fields_the_judgement_needs() {
        let record = OrderRecord {
            client_order_id: cloid("0x01"),
            unit: Unit::Batch(Direction::Long, BatchIndex(2)),
            purpose: OrderPurpose::Core,
            limit_price: Price::parse("85000").unwrap(),
            quantity: Quantity::parse("0.175").unwrap(),
            cumulative_filled: Quantity::parse("0.07").unwrap(),
            reduce_only: false,
            status: OrderStatus::PartiallyFilled,
        };

        let state = OrderState::of(&record, false);

        assert_eq!(state.client_order_id, cloid("0x01"));
        assert_eq!(state.unit, Unit::Batch(Direction::Long, BatchIndex(2)));
        assert_eq!(state.purpose, OrderPurpose::Core);
        assert_eq!(state.cumulative_filled, Quantity::parse("0.07").unwrap());
        assert!(!state.fills_reconciled);
    }

    #[test]
    fn a_matching_position_is_consistent() {
        assert_eq!(
            position_check(&reading("0.05", "0.05"), None),
            PositionCheck::Consistent
        );
        // 对上了就是结论，不必再看数据有没有稳定。
        assert_eq!(
            position_check(&reading("0.05", "0.05"), Some(&reading("0.04", "0.05"))),
            PositionCheck::Consistent
        );
    }

    #[test]
    fn data_that_changed_since_the_last_round_is_unstable() {
        assert_eq!(
            position_check(&reading("0.04", "0.03"), None),
            PositionCheck::Unstable
        );
        assert_eq!(
            position_check(&reading("0.04", "0.03"), Some(&reading("0.03", "0.03"))),
            PositionCheck::Unstable
        );
    }

    #[test]
    fn the_same_mismatch_twice_is_a_mismatch() {
        let same = reading("0.01", "0");
        assert_eq!(position_check(&same, Some(&same)), PositionCheck::Mismatch);
    }

    /// 净仓位一模一样、订单快照却在变：不能算「同一份数据」。
    #[test]
    fn a_standing_net_position_is_not_stable_while_the_order_snapshot_moves() {
        let before = PositionReading {
            actual: Position::parse("0.03").unwrap(),
            expected: Position::parse("0.02").unwrap(),
            reported: vec![(cloid("0x01"), Quantity::parse("0.02").unwrap())],
        };
        let after = PositionReading {
            reported: vec![(cloid("0x01"), Quantity::parse("0.03").unwrap())],
            ..before.clone()
        };

        assert_eq!(
            position_check(&after, Some(&before)),
            PositionCheck::Unstable
        );
        assert_eq!(
            position_check(&after, Some(&after)),
            PositionCheck::Mismatch
        );
    }

    #[test]
    fn the_expected_position_adds_the_unrecorded_fills() {
        // 格子状态算出应有持仓 .05；交易所另外报了买入 .02 还没入账。
        let orders = [
            cell_order("0", true),
            order(
                "0x02",
                Unit::Cell(Direction::Long, CellIndex(1)),
                OrderPurpose::GridEntry,
                "0",
                true,
            ),
        ];
        let reported = vec![(cloid("0x01"), Quantity::parse("0.02").unwrap())];

        assert_eq!(
            expected_position(Position::parse("0.05").unwrap(), &reported, &orders),
            Position::parse("0.07").unwrap()
        );
    }

    #[test]
    fn a_sell_that_is_not_recorded_yet_comes_off_the_expected_position() {
        let orders = [order(
            "0x01",
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridExit,
            "0",
            true,
        )];
        let reported = vec![(cloid("0x01"), Quantity::parse("0.05").unwrap())];

        assert_eq!(
            expected_position(Position::parse("0.05").unwrap(), &reported, &orders),
            Position::ZERO
        );
    }

    /// 补记成交之后我们的记录可能已经追上了快照：差额按 0 算，不能变成负数。
    #[test]
    fn a_snapshot_behind_our_records_contributes_nothing() {
        let orders = [cell_order("0.05", true)];
        let reported = vec![(cloid("0x01"), Quantity::parse("0.02").unwrap())];

        assert_eq!(
            expected_position(Position::parse("0.05").unwrap(), &reported, &orders),
            Position::parse("0.05").unwrap()
        );
    }

    /// 快照里有一张我们不认识的订单（例如用户手动交易）：它不参与推算。
    #[test]
    fn a_snapshot_for_an_unknown_order_is_ignored() {
        let reported = vec![(cloid("0xforeign"), Quantity::parse("0.05").unwrap())];

        assert_eq!(
            expected_position(Position::parse("0.05").unwrap(), &reported, &[]),
            Position::parse("0.05").unwrap()
        );
    }

    #[test]
    fn only_orders_whose_fills_are_unsettled_block_their_unit() {
        let orders = [
            cell_order("0.02", false),
            order(
                "0x02",
                Unit::Batch(Direction::Long, BatchIndex(1)),
                OrderPurpose::Core,
                "0.175",
                true,
            ),
            order(
                "0x03",
                Unit::Cell(Direction::Long, CellIndex(1)),
                OrderPurpose::GridExit,
                "0.01",
                false,
            ),
        ];

        assert_eq!(
            pending_units(&orders),
            vec![
                PendingUnit {
                    unit: Unit::Cell(Direction::Long, CellIndex(0)),
                    purpose: OrderPurpose::GridEntry,
                },
                PendingUnit {
                    unit: Unit::Cell(Direction::Long, CellIndex(1)),
                    purpose: OrderPurpose::GridExit,
                },
            ]
        );
    }

    #[test]
    fn a_settled_order_needs_both_sides_to_agree() {
        let recorded = Quantity::parse("0.05").unwrap();
        let saved = Quantity::parse("0.05").unwrap();

        assert!(fills_settled(recorded, recorded, saved));
        // 交易所报得更多：还有成交没入账。
        assert!(!fills_settled(
            recorded,
            Quantity::parse("0.06").unwrap(),
            saved
        ));
        // 流水少了：真实缺失的那笔还没到。
        assert!(!fills_settled(
            recorded,
            recorded,
            Quantity::parse("0.04").unwrap()
        ));
        // 流水多了：重复报告不能算到齐。
        assert!(!fills_settled(
            recorded,
            recorded,
            Quantity::parse("0.07").unwrap()
        ));
    }

    #[test]
    fn unrecorded_quantity_never_goes_negative() {
        assert_eq!(
            unbooked_quantity(
                Quantity::parse("0.05").unwrap(),
                Quantity::parse("0.02").unwrap()
            ),
            Quantity::parse("0.03").unwrap()
        );
        assert_eq!(
            unbooked_quantity(
                Quantity::parse("0.02").unwrap(),
                Quantity::parse("0.05").unwrap()
            ),
            Quantity::ZERO
        );
    }

    #[test]
    fn fill_records_above_the_recorded_cumulative_are_reported() {
        let mut saved = HashMap::new();
        saved.insert(cloid("0x01"), Quantity::parse("0.02").unwrap());
        let orders = [cell_order("0.02", false)];

        // 相等：正常的数据。
        assert_eq!(inconsistent_fill_records(&saved, &orders), None);

        // 流水多出来一笔：同一笔成交换了成交编号再报一次。
        saved.insert(cloid("0x01"), Quantity::parse("0.04").unwrap());
        assert_eq!(
            inconsistent_fill_records(&saved, &orders),
            Some(cloid("0x01"))
        );
    }

    #[test]
    fn a_matching_position_with_no_inconsistency_proceeds() {
        let decision = judge(PositionCheck::Consistent, None, &[cell_order("0.02", true)]);

        assert_eq!(decision.verdict, Verdict::Proceed);
        assert!(decision.blocked.is_empty());
    }

    #[test]
    fn a_stable_mismatch_pauses_the_strategy() {
        let decision = judge(PositionCheck::Mismatch, None, &[]);

        assert_eq!(
            decision.verdict,
            Verdict::Pause(PauseReason::PositionCheckFailed)
        );
        assert_eq!(
            PauseReason::PositionCheckFailed.wait_reason(),
            WaitReason::PositionCheckFailed
        );
    }

    #[test]
    fn data_that_is_still_moving_waits_for_the_next_step() {
        let decision = judge(PositionCheck::Unstable, None, &[]);

        assert_eq!(
            decision.verdict,
            Verdict::Wait(WaitReason::PositionUnstable)
        );
    }

    #[test]
    fn inconsistent_fill_records_pause_the_strategy() {
        let decision = judge(PositionCheck::Consistent, Some(&cloid("0x01")), &[]);

        assert_eq!(
            decision.verdict,
            Verdict::Pause(PauseReason::FillRecordsInconsistent)
        );
        assert_eq!(
            PauseReason::FillRecordsInconsistent.wait_reason(),
            WaitReason::FillRecordsInconsistent
        );
    }

    /// 数据还在变的时候，成交记录对不上可能只是流水还没到：先按「等待」处理。
    #[test]
    fn a_position_that_is_still_moving_hides_the_inconsistency() {
        let decision = judge(PositionCheck::Unstable, Some(&cloid("0x01")), &[]);

        assert_eq!(
            decision.verdict,
            Verdict::Wait(WaitReason::PositionUnstable)
        );
    }

    /// 结论里始终带着被阻塞的单位：暂停和等待的路径也一样。
    #[test]
    fn every_verdict_carries_the_blocked_units() {
        let orders = [cell_order("0.02", false)];
        let blocked = vec![PendingUnit {
            unit: Unit::Cell(Direction::Long, CellIndex(0)),
            purpose: OrderPurpose::GridEntry,
        }];

        assert_eq!(
            judge(PositionCheck::Consistent, None, &orders).blocked,
            blocked
        );
        assert_eq!(
            judge(PositionCheck::Unstable, None, &orders).blocked,
            blocked
        );
        assert_eq!(
            judge(PositionCheck::Mismatch, None, &orders).blocked,
            blocked
        );
    }
}
