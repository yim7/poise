//! 判定：拿一份从交易所读回来的快照，说这一步能不能继续下单。
//!
//! 纯计算，不读时钟、不访问网络（第 12 节）。向交易所取数的 I/O 在 runtime 的读取
//! 阶段，这里只处理读回来的数据，所以可以直接用数据测试。
//!
//! 输入有四样：仓位核对的结论、成交流水与订单累计成交量是否对得上、本地订单快照、
//! 系统状态（第 7.2 节的闸门）。输出是一个 [`Decision`]：暂停原因、这一步允许发出的
//! 新订单（加仓、减仓分开给，撤单始终允许）、等待原因，以及哪些单位的哪些用途不能
//! 下单（第 7.1、7.2、9.2 节）。
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

/// 系统状态闸门的输入（第 7.2 节）：这一步读到的、和单个单位无关的系统状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemState {
    /// 本策略有订单结果未知：交易所查不到它，又还不能判定它没有任何影响（第 9.2 节）。
    pub unknown_orders: bool,
    /// 账户保证金率的状态（第 7.2、7.4 节）。
    pub margin: MarginState,
}

/// 账户保证金率的状态。保证金是整个账户共用的，同一账户的所有策略看到的是同一个状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MarginState {
    /// 读到了，没有超过停止阈值。
    #[default]
    Normal,
    /// 读不到或数据过期：不发任何新单，已在场的挂单保留（第 7.2 节）。
    Unavailable,
    /// 超过停止阈值：只发、只保留只减仓单，其余挂单撤掉；回落后自动恢复（第 7.4 节）。
    AboveStop,
}

/// 这一步允许发出哪些新订单（第 7.2 节）。
///
/// 撤单不在这里：撤单始终允许。闸门关上时只是不发新单，已经规划好的撤单照常执行——
/// 用一个「等待」提前结束这一步，就会把该撤的单也一起跳过。
///
/// 加仓和减仓分开给：保证金率超过停止阈值时只能发只减仓单（第 7.4 节），系统状态不对
/// 时两种都不能发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permits {
    /// 不是只减仓的订单：主仓买入、格子的买回或开仓，以及中性网格里可能让净仓位穿越零的
    /// 格子单。
    pub increase: bool,
    /// 只减仓的订单。
    pub reduce: bool,
}

impl Permits {
    pub const ALL: Self = Self {
        increase: true,
        reduce: true,
    };
    pub const NONE: Self = Self {
        increase: false,
        reduce: false,
    };

    /// 这一步能不能发出这样一张新订单。
    pub const fn allows(self, reduce_only: bool) -> bool {
        if reduce_only {
            self.reduce
        } else {
            self.increase
        }
    }
}

/// 一次判定的完整结果。
///
/// 除了「能发哪些新单」，还带上所有没对完账的单位与用途：撤单也会让订单失去对账，
/// 撤单之后要按这一份重新过滤规划出来的订单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// 要暂停策略时的原因：撤销全部挂单，交回用户。
    pub pause: Option<PauseReason>,
    /// 这一步允许发出的新订单。暂停时两种都不允许。
    pub permits: Permits,
    /// 这一步为什么不能（全部）照常下单，记进规划的等待原因。
    pub waiting: Vec<WaitReason>,
    /// 在场的非只减仓单要不要撤掉：保证金率超过停止阈值时，账户只保留只减仓单（第 7.4 节）。
    /// 其他关闸的情况只是不发新单，在场的挂单保留。
    pub withdraw_increasing: bool,
    /// 还没有对完账、这一侧不能下新单的单位与用途。
    pub blocked: Vec<PendingUnit>,
}

/// 判定这一步能发哪些新订单。
///
/// 仓位核对的问题优先于成交记录：数据还在变的时候，成交记录对不上可能只是流水还没到；
/// 仓位已经稳定且对不上时，再报成交记录的问题只会把注意力引偏。
///
/// 暂停之外的情况都不提前结束这一步：仓位数据还在变、有订单结果未知时只是不发新单，
/// 规划出来的撤单照常执行（第 7.2 节）。
pub fn judge(
    position: PositionCheck,
    inconsistent: Option<&ClientOrderId>,
    orders: &[OrderState],
    system: SystemState,
) -> Decision {
    let blocked = pending_units(orders);
    let pause = match position {
        PositionCheck::Mismatch => Some(PauseReason::PositionCheckFailed),
        PositionCheck::Consistent if inconsistent.is_some() => {
            Some(PauseReason::FillRecordsInconsistent)
        }
        _ => None,
    };
    if let Some(reason) = pause {
        return Decision {
            pause,
            permits: Permits::NONE,
            waiting: vec![reason.wait_reason()],
            withdraw_increasing: false,
            blocked,
        };
    }

    // 数据还在变、有订单结果未知、保证金率读不到：两种新单都不发。
    let mut waiting = Vec::new();
    if position == PositionCheck::Unstable {
        waiting.push(WaitReason::PositionUnstable);
    }
    if system.unknown_orders {
        waiting.push(WaitReason::OrderResultUnknown);
    }
    if system.margin == MarginState::Unavailable {
        waiting.push(WaitReason::MarginRatioUnavailable);
    }
    let mut permits = if waiting.is_empty() {
        Permits::ALL
    } else {
        Permits::NONE
    };
    // 保证金率超过停止阈值：只减仓单照常（单侧网格的格子平仓单就是），其余的不发也不留。
    let withdraw_increasing = system.margin == MarginState::AboveStop;
    if withdraw_increasing {
        waiting.push(WaitReason::MarginRatioAboveStopThreshold);
        permits.increase = false;
    }
    Decision {
        pause: None,
        permits,
        waiting,
        withdraw_increasing,
        blocked,
    }
}

/// 一张结果未知的订单还缺哪些事实，才能判定它没有任何影响（第 9.2 节第 3 条）。
///
/// 交易所查不到它（`unknownOid`）之后，要同时满足三项：期限（`expiresAfter` 加安全余量）
/// 已过、从发送到现在的完整成交里没有它的成交、仓位与应有持仓一致。「当前挂单中没有它」
/// 由调用方保证：只有挂单列表和按 `cloid` 查询都找不到的订单才会走到这里。
///
/// 这条规则只适用于交易所从没确认过的订单（下单超时、写库后没来得及发送）。`expiresAfter`
/// 只限制请求送达的时间，不会撤销已经挂上的单：交易所确认过它在场、后来又查不到的，
/// 可能只是查询滞后，它仍然可能成交，只能等交易所给出明确的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingFact {
    /// 交易所确认过它在场（拿到过交易所的订单编号）：它落过地，不是「从没生效」。
    NeverConfirmed,
    /// 还没过期限：请求可能还在路上，随时可能落地。
    Deadline,
    /// 它有成交记录，或者本地已经记过它的成交：它落过地。
    Fills,
    /// 仓位与应有持仓不一致，或者还没核对出结论。
    Position,
}

/// 列出一张查不到的订单还缺的事实；返回空表示可以判定它没有任何影响、释放预留。
pub fn missing_facts(
    confirmed_by_exchange: bool,
    deadline_passed: bool,
    recorded: Quantity,
    saved_fills: Quantity,
    position: PositionCheck,
) -> Vec<MissingFact> {
    let mut missing = Vec::new();
    if confirmed_by_exchange {
        missing.push(MissingFact::NeverConfirmed);
    }
    if !deadline_passed {
        missing.push(MissingFact::Deadline);
    }
    if !recorded.is_zero() || !saved_fills.is_zero() {
        missing.push(MissingFact::Fills);
    }
    if position != PositionCheck::Consistent {
        missing.push(MissingFact::Position);
    }
    missing
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
/// 由格子状态算出的应有持仓，减去失效减仓和市价平仓已经记入的成交（它们不改格子状态，见
/// [`crate::Strategy::apply_fill`]），再加上交易所已经成交、我们还没记入的数量：成交记录
/// 可能比挂单状态晚到，这段差额是能解释的（第 7.1.3 节）。
pub fn expected_position(
    base: Position,
    reported: &[(ClientOrderId, Quantity)],
    orders: &[OrderState],
) -> Position {
    let mut unrecorded: i64 = orders
        .iter()
        .filter(|order| order.purpose.is_reduction())
        .map(|order| signed_delta(order.unit.side(order.purpose), order.cumulative_filled))
        .sum();
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
/// 典型的是撤单期间的部分成交：撤销只说明订单不在场了，交易所给出最终累计量之前，
/// 按旧数量补单就会多买或多卖（第 9.2 节）。
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
/// 记账以订单状态为准，流水被记到错的订单上不会直接改格子；但它说明数据本身有问题，
/// 继续自动运行没有意义，暂停交给用户处理。
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

/// 一张订单的成交是否已经对完账：本地记入的累计成交量追上了交易所给的累计成交量。
///
/// 调用方要先拿到交易所对这张单的说法（挂单列表或按 `cloid` 查询）。累计成交量以它为准，
/// 成交流水只是审计记录：流水晚到、漏掉都不影响记账，所以不在这里等它（第 9.3 节）。
///
/// 没对完账的订单继续占用所属单位：规划时不为这个单位下同一用途的新单。撤单之后最终
/// 累计量还不知道，按撤单前的数量补一张就会多买或多卖（第 9.2 节）。
pub fn fills_settled(recorded: Quantity, exchange_cumulative: Quantity) -> bool {
    unbooked_quantity(exchange_cumulative, recorded).is_zero()
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
        Decision, MarginState, MissingFact, OrderState, PauseReason, Permits, PositionCheck,
        PositionReading, SystemState, expected_position, fills_settled, inconsistent_fill_records,
        judge, missing_facts, pending_units, position_check, unbooked_quantity,
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

    /// 观点失效的减仓成交不进账本（见 `Strategy::apply_fill`）：应有持仓直接扣掉它记入的
    /// 累计成交量。迟到的增仓成交先记还是后记，结果都一样。
    #[test]
    fn recorded_invalidation_fills_come_off_the_expected_position() {
        let sold = OrderState {
            client_order_id: ClientOrderId::try_new("0xinv").unwrap(),
            unit: Unit::Side(crate::side::Direction::Long),
            purpose: OrderPurpose::Invalidation,
            cumulative_filled: Quantity::parse("1").unwrap(),
            fills_reconciled: true,
        };

        // 账上 .6、迟到的 .4 还没记入：交易所报了它的累计量，按未记入计入。
        let late = OrderState {
            client_order_id: ClientOrderId::try_new("0xlate").unwrap(),
            unit: Unit::Batch(crate::side::Direction::Long, crate::batch::BatchIndex(1)),
            purpose: OrderPurpose::Core,
            cumulative_filled: Quantity::ZERO,
            fills_reconciled: false,
        };
        let reported = [(
            late.client_order_id.clone(),
            Quantity::parse("0.4").unwrap(),
        )];
        assert_eq!(
            expected_position(
                Position::parse("0.6").unwrap(),
                &reported,
                &[sold.clone(), late.clone()]
            ),
            Position::ZERO
        );
        // 迟到的 .4 记入之后：账上 1，同样是 0。
        let booked = OrderState {
            cumulative_filled: Quantity::parse("0.4").unwrap(),
            fills_reconciled: true,
            ..late
        };
        assert_eq!(
            expected_position(Position::parse("1").unwrap(), &reported, &[sold, booked]),
            Position::ZERO
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

    /// 对完账只看交易所的累计量有没有记进来，不等流水。
    #[test]
    fn an_order_is_settled_once_the_exchange_cumulative_is_booked() {
        let recorded = Quantity::parse("0.05").unwrap();

        assert!(fills_settled(recorded, recorded));
        // 交易所报得更多：还有成交没入账。
        assert!(!fills_settled(recorded, Quantity::parse("0.06").unwrap()));
        // 交易所的快照落后于我们（流水先到了）：已经记过的不会少。
        assert!(fills_settled(recorded, Quantity::parse("0.04").unwrap()));
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

    fn judge_plain(position: PositionCheck, inconsistent: Option<&ClientOrderId>) -> Decision {
        judge(position, inconsistent, &[], SystemState::default())
    }

    #[test]
    fn a_matching_position_with_no_inconsistency_permits_everything() {
        let decision = judge(
            PositionCheck::Consistent,
            None,
            &[cell_order("0.02", true)],
            SystemState::default(),
        );

        assert_eq!(decision.pause, None);
        assert_eq!(decision.permits, Permits::ALL);
        assert!(decision.waiting.is_empty());
        assert!(decision.blocked.is_empty());
    }

    #[test]
    fn a_stable_mismatch_pauses_the_strategy() {
        let decision = judge_plain(PositionCheck::Mismatch, None);

        assert_eq!(decision.pause, Some(PauseReason::PositionCheckFailed));
        assert_eq!(decision.permits, Permits::NONE);
        assert_eq!(decision.waiting, vec![WaitReason::PositionCheckFailed]);
    }

    /// 数据还在变：这一步不发新单，但不暂停——规划出来的撤单照常执行。
    #[test]
    fn data_that_is_still_moving_permits_no_new_orders() {
        let decision = judge_plain(PositionCheck::Unstable, None);

        assert_eq!(decision.pause, None);
        assert_eq!(decision.permits, Permits::NONE);
        assert_eq!(decision.waiting, vec![WaitReason::PositionUnstable]);
    }

    #[test]
    fn inconsistent_fill_records_pause_the_strategy() {
        let decision = judge_plain(PositionCheck::Consistent, Some(&cloid("0x01")));

        assert_eq!(decision.pause, Some(PauseReason::FillRecordsInconsistent));
        assert_eq!(decision.waiting, vec![WaitReason::FillRecordsInconsistent]);
    }

    /// 数据还在变的时候，成交记录对不上可能只是流水还没到：先按「等待」处理。
    #[test]
    fn a_position_that_is_still_moving_hides_the_inconsistency() {
        let decision = judge_plain(PositionCheck::Unstable, Some(&cloid("0x01")));

        assert_eq!(decision.pause, None);
        assert_eq!(decision.waiting, vec![WaitReason::PositionUnstable]);
    }

    /// B36：有订单结果未知时本策略不发任何新订单，加仓、减仓都不行；但不暂停，
    /// 已在场的挂单保留（第 7.2 节）。
    #[test]
    fn b36_an_unknown_order_closes_the_gate_without_pausing() {
        let decision = judge(
            PositionCheck::Consistent,
            None,
            &[],
            SystemState {
                unknown_orders: true,
                ..SystemState::default()
            },
        );

        assert_eq!(decision.pause, None);
        assert_eq!(decision.permits, Permits::NONE);
        assert_eq!(decision.waiting, vec![WaitReason::OrderResultUnknown]);
    }

    /// 仓位对不上时暂停优先：闸门只是不发新单，仓位核对失败要撤掉全部挂单。
    #[test]
    fn a_mismatch_pauses_even_when_the_gate_is_closed() {
        let decision = judge(
            PositionCheck::Mismatch,
            None,
            &[],
            SystemState {
                unknown_orders: true,
                ..SystemState::default()
            },
        );

        assert_eq!(decision.pause, Some(PauseReason::PositionCheckFailed));
    }

    #[test]
    fn permits_tell_reduce_only_orders_apart() {
        let reduce_only = Permits {
            increase: false,
            reduce: true,
        };

        assert!(reduce_only.allows(true));
        assert!(!reduce_only.allows(false));
        assert!(Permits::ALL.allows(false));
        assert!(!Permits::NONE.allows(true));
    }

    /// 结论里始终带着被阻塞的单位：暂停和等待的路径也一样。
    #[test]
    fn every_decision_carries_the_blocked_units() {
        let orders = [cell_order("0.02", false)];
        let blocked = vec![PendingUnit {
            unit: Unit::Cell(Direction::Long, CellIndex(0)),
            purpose: OrderPurpose::GridEntry,
        }];

        for position in [
            PositionCheck::Consistent,
            PositionCheck::Unstable,
            PositionCheck::Mismatch,
        ] {
            assert_eq!(
                judge(position, None, &orders, SystemState::default()).blocked,
                blocked
            );
        }
    }

    /// B15：期限已过、没有成交、仓位一致，三项都有了才能判定没有影响。
    #[test]
    fn b15_an_order_with_all_three_facts_has_no_effect() {
        assert!(
            missing_facts(
                false,
                true,
                Quantity::ZERO,
                Quantity::ZERO,
                PositionCheck::Consistent
            )
            .is_empty()
        );
    }

    /// B36：保证金率读不到时不发任何新单，已在场的挂单保留。
    #[test]
    fn b36_an_unreadable_margin_ratio_closes_the_gate() {
        let decision = judge(
            PositionCheck::Consistent,
            None,
            &[],
            SystemState {
                margin: MarginState::Unavailable,
                ..SystemState::default()
            },
        );

        assert_eq!(decision.permits, Permits::NONE);
        assert!(!decision.withdraw_increasing);
        assert_eq!(decision.waiting, vec![WaitReason::MarginRatioUnavailable]);
    }

    /// B35：保证金率超过停止阈值时只发、只保留只减仓单。
    #[test]
    fn b35_a_margin_ratio_above_the_stop_threshold_keeps_only_reduce_only_orders() {
        let decision = judge(
            PositionCheck::Consistent,
            None,
            &[],
            SystemState {
                margin: MarginState::AboveStop,
                ..SystemState::default()
            },
        );

        assert_eq!(
            decision.permits,
            Permits {
                increase: false,
                reduce: true,
            }
        );
        assert!(decision.withdraw_increasing);
        assert_eq!(
            decision.waiting,
            vec![WaitReason::MarginRatioAboveStopThreshold]
        );
    }

    /// 超过停止阈值又有订单结果未知：只减仓单也不发，但非只减仓单照样撤。
    #[test]
    fn an_unknown_order_also_stops_reduce_only_orders_above_the_stop_threshold() {
        let decision = judge(
            PositionCheck::Consistent,
            None,
            &[],
            SystemState {
                unknown_orders: true,
                margin: MarginState::AboveStop,
            },
        );

        assert_eq!(decision.permits, Permits::NONE);
        assert!(decision.withdraw_increasing);
    }

    /// B16：任一项不满足就不能释放，并说清楚缺的是哪一项。
    #[test]
    fn b16_each_missing_fact_is_reported() {
        let some = Quantity::parse("0.01").unwrap();

        assert_eq!(
            missing_facts(
                false,
                false,
                Quantity::ZERO,
                Quantity::ZERO,
                PositionCheck::Consistent
            ),
            vec![MissingFact::Deadline]
        );
        assert_eq!(
            missing_facts(false, true, Quantity::ZERO, some, PositionCheck::Consistent),
            vec![MissingFact::Fills]
        );
        assert_eq!(
            missing_facts(false, true, some, Quantity::ZERO, PositionCheck::Consistent),
            vec![MissingFact::Fills]
        );
        assert_eq!(
            missing_facts(
                false,
                true,
                Quantity::ZERO,
                Quantity::ZERO,
                PositionCheck::Unstable
            ),
            vec![MissingFact::Position]
        );
        assert_eq!(
            missing_facts(false, false, some, Quantity::ZERO, PositionCheck::Mismatch),
            vec![
                MissingFact::Deadline,
                MissingFact::Fills,
                MissingFact::Position
            ]
        );
    }

    /// 交易所确认过在场的订单不按「从没生效」作废：`expiresAfter` 不撤销已经挂上的单，
    /// 后来查不到可能只是查询滞后。
    #[test]
    fn an_order_the_exchange_once_confirmed_is_never_voided() {
        assert_eq!(
            missing_facts(
                true,
                true,
                Quantity::ZERO,
                Quantity::ZERO,
                PositionCheck::Consistent
            ),
            vec![MissingFact::NeverConfirmed]
        );
    }
}
