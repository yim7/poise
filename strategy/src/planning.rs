//! 规划：给定状态、在场订单和当前价格，输出要下的单、要撤的单和等待原因。
//!
//! 规则（每一侧都一样，只是买卖方向相反）：
//! - 每批主仓在自己的批次价挂建仓单，不追价；成交后格子按比例启用（第 4.2 节）；
//! - 启用后的格子：持有部分在平仓价挂平仓单，待买回部分在建仓价挂建仓单；
//! - 还没被限速放行的单位不挂新单（第 4.1 节）；
//! - 某一侧剩余量不足最低订单金额时，这一侧先不挂单，只挂另一侧（第 4.4 节）；
//! - 新订单的限价不与本策略在场的反向订单相交（第 6 节），两侧一起看。
//!
//! 限速只挡住「新单位的第一次挂单」：已启用格子的买卖循环不受限速影响（第 4.3 节），
//! 也不会因为等待放行而释放任何预留。运行中改成限速会让还没开始的单位回到未放行
//! （第 4.1 节），未放行的单位这一步不挂新单，也不撤它在场的订单；已经结束的批次不受
//! 放行影响，它的主仓建仓单照撤。
//!
//! 中性网格两侧的格子平仓单不标只减仓（第 5.2 节）：一侧的平仓在交易所层面可能就是
//! 在减另一侧的仓位。

use serde::{Deserialize, Serialize};

use crate::Strategy;
use crate::batch::{Batch, BatchIndex};
use crate::grid::Cell;
use crate::instrument::PriceRule;
use crate::order::{ClientOrderId, OrderPurpose, Side, Unit, WorkingOrder};
use crate::side::{Direction, SidePlan};
use crate::state::{BatchState, CellState, StrategyState};
use crate::units::{Price, Quantity};

/// 要下的一张订单：身份和客户端订单 ID 由 runtime 生成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderIntent {
    /// 归属单位：一批主仓或一个格子。
    pub unit: Unit,
    pub purpose: OrderPurpose,
    pub side: Side,
    pub limit_price: Price,
    pub quantity: Quantity,
    pub reduce_only: bool,
}

/// 一个还没有对完账的单位：这个单位的这一侧暂时不下新单。
///
/// 「对完账」指订单已经有交易所的权威结局（终态或仍在场）、本地记入的累计成交量
/// 追上了交易所报的累计成交量，而且成交流水也追上了记入的累计成交量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingUnit {
    pub unit: Unit,
    pub purpose: OrderPurpose,
}

/// 策略当前不发出新订单的原因。
///
/// 除最后三条外是设计第 11 节列出的集合。本任务支持的配置（做多、可选限速）会产生
/// 「等待限速放行」「等待所属批次」和「限价因自身反向订单下调」三种；其余属于后续任务。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// 价格未到。
    PriceNotReached,
    /// 等待限速放行。
    WaitingForThrottle,
    /// 等待所属批次。
    WaitingForBatch,
    /// 限价因自身反向订单下调。
    LimitAdjustedByOwnOrder,
    /// 资金不足或退避中。
    InsufficientFunds,
    /// 保证金率超过停止阈值。
    MarginRatioAboveStopThreshold,
    /// 保证金率读不到或数据过期：不发新单，已在场的挂单保留（第 7.2 节）。
    MarginRatioUnavailable,
    /// 行情过期。
    StaleMarketData,
    /// 订单结果待查。
    OrderResultUnknown,
    /// 仓位核对失败已暂停。
    PositionCheckFailed,
    /// 观点已失效。
    Invalidated,
    /// 这个单位还有订单的成交没对完账：先只保留预留，不下新单。
    ///
    /// 设计第 11 节没有列这一条，这里是新增的。
    FillsPending,
    /// 仓位数据还在变化，这一轮核对说明不了问题：下一步再核对。
    ///
    /// 设计第 11 节没有列这一条，这里是新增的。
    PositionUnstable,
    /// 成交流水与订单累计成交量对不上：按成交编号去重后的数量之和超过了已记入的
    /// 累计成交量。正常数据不会这样，继续自动重试没有意义，暂停交给用户处理。
    ///
    /// 设计第 11 节没有列这一条，这里是新增的。
    FillRecordsInconsistent,
}

/// 一次规划的完整结果。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Plan {
    /// 做出这次规划时的标记价。
    pub market_price: Price,
    pub place: Vec<OrderIntent>,
    pub cancel: Vec<ClientOrderId>,
    pub waiting: Vec<WaitReason>,
}

/// 一个单位某一侧应当在场的一张订单。
struct Demand {
    unit: Unit,
    purpose: OrderPurpose,
    /// 这条腿的买卖方向：由用途和这一侧的方向决定（第 5.1 节）。
    side: Side,
    /// 已经按第 6 节调整过的限价。
    limit_price: Price,
    quantity: Quantity,
    reduce_only: bool,
    /// 这个单位的这一侧还有订单的成交没对完账：在场订单照常处理（不再需要的撤掉），
    /// 但不补新单。撤单前的最终成交量还不知道，按旧数量补一张就会多买或多卖。
    blocked: bool,
}

/// 一个单位的某一侧对在场订单的处理结果。
enum Placement {
    /// 在场订单已经够了，或者缺的部分已经挂出。
    Handled,
    /// 缺的部分不足最低订单金额，没挂（第 4.4 节）。
    BelowMinimum,
    /// 缺的部分因为成交还没对完账没挂。
    Blocked,
}

/// 本策略在场订单给出的限价边界（第 6 节）。
///
/// 新订单的限价不得与本策略在场的反向订单相交：买单最高到「在场卖单最低价 −
/// 一个报价单位」，卖单最低到「在场买单最高价 + 一个报价单位」。调整只会让
/// 买单更低、卖单更高，不会劣于原价。
///
/// 「一个报价单位」是那张反向订单价位上的最小变动：报价规则随价格变粗，
/// 按粗步长退开的价格是细步长的整数倍，在两边都合规。
#[derive(Debug, Default)]
struct OwnOrderBounds {
    lowest_sell: Option<Price>,
    highest_buy: Option<Price>,
}

impl OwnOrderBounds {
    fn of(working_orders: &[WorkingOrder]) -> Self {
        let mut bounds = Self::default();
        for order in working_orders {
            bounds.include(order.unit.side(order.purpose), order.limit_price);
        }
        bounds
    }

    /// 记下一张订单的限价：在场订单和这一步刚规划出去的订单都要算。
    ///
    /// 同一批规划里下出的两张单也会同时挂在市场上，彼此同样不能相交。
    fn include(&mut self, side: Side, limit_price: Price) {
        match side {
            Side::Sell => {
                self.lowest_sell = Some(
                    self.lowest_sell
                        .map_or(limit_price, |price| price.min(limit_price)),
                );
            }
            Side::Buy => {
                self.highest_buy = Some(
                    self.highest_buy
                        .map_or(limit_price, |price| price.max(limit_price)),
                );
            }
        }
    }

    fn buy_limit(&self, desired: Price, rule: PriceRule) -> Price {
        match self.lowest_sell {
            Some(lowest_sell) => desired.min(lowest_sell.saturating_sub(rule.step_at(lowest_sell))),
            None => desired,
        }
    }

    fn sell_limit(&self, desired: Price, rule: PriceRule) -> Price {
        match self.highest_buy {
            Some(highest_buy) => desired.max(highest_buy.saturating_add(rule.step_at(highest_buy))),
            None => desired,
        }
    }
}

/// 一次规划的中间状态。
///
/// 主仓和格子两段共用同一份在场订单边界和同一个 [`Plan`]：同一步规划出去的订单也会
/// 同时挂在市场上，后规划的那一侧必须看得到它，否则两张反向单会相交（第 6 节）。
struct Planner<'a> {
    strategy: &'a Strategy,
    state: &'a StrategyState,
    working_orders: &'a [WorkingOrder],
    /// 还没有对完账的单位：这些单位的这一侧不补新单。
    pending: &'a [PendingUnit],
    plan: Plan,
    /// 在场订单给出的限价边界（第 6 节），随规划过程更新。
    bounds: OwnOrderBounds,
    /// 报价规则；让新限价退开一个报价单位时用。
    rule: PriceRule,
    waiting_for_fills: bool,
    waiting_for_throttle: bool,
    waiting_for_batch: bool,
}

impl<'a> Planner<'a> {
    fn new(
        strategy: &'a Strategy,
        state: &'a StrategyState,
        working_orders: &'a [WorkingOrder],
        pending: &'a [PendingUnit],
        market_price: Price,
    ) -> Self {
        Self {
            strategy,
            state,
            working_orders,
            pending,
            plan: Plan {
                market_price,
                ..Plan::default()
            },
            bounds: OwnOrderBounds::of(working_orders),
            rule: strategy.spec().price_rule(),
            waiting_for_fills: false,
            waiting_for_throttle: false,
            waiting_for_batch: false,
        }
    }

    /// 两侧：先格子、后主仓。
    ///
    /// 顺序决定同一步里反向订单谁让谁：先规划的单按原价挂，后规划的单避开它（第 6 节）。
    /// 格子在前，持有格子的平仓单留在平仓价上——那是网格的利润所在；主仓建仓单在后，
    /// 和它们相交时往外让，晚一点买到没关系。接管已有仓位之后正是这种情形：持有的格子
    /// 卖单在场，本批主仓还没买完（第 6 节的例子）。
    fn plan_sides(&mut self) {
        for side in self.strategy.sides() {
            self.plan_cells(side);
            self.plan_batches(side);
        }
    }

    /// 一侧的主仓：每批在自己的批次价挂建仓单，不追价。
    fn plan_batches(&mut self, side: &SidePlan) {
        let direction = side.direction();
        for batch in side.batches().batches() {
            let Some(batch_state) = self.batch_state(direction, batch.index) else {
                continue;
            };
            let unit = Unit::Batch(direction, batch.index);
            // 已经结束的批次先处理：它不再建仓，在场的建仓单必须撤掉。这一步不能等放行
            // ——结束的批次不会再成交，等它「重新放行」可能永远等不到，留着那张单就会在
            // 本批已经决定不再建仓之后又建进来。
            if batch_state.finished {
                self.cancel_core_orders(unit);
                continue;
            }
            // 还没放行的批次不挂单。启动时它本来没有在场订单，但运行中从不限速改成限速
            // 会让还没开始的批次回到未放行（第 4.1 节），那种情况下它的在场订单留在原地，
            // 重新放行之后再处理。
            if !self.strategy.is_released(self.state, unit) {
                self.waiting_for_throttle = true;
                continue;
            }
            self.plan_batch(direction, batch, batch_state);
        }
    }

    /// 一批主仓的建仓单：本批最多能在场的数量是 `C − F`，在场订单已经有多少由
    /// [`Strategy::reconcile`] 判断。
    fn plan_batch(&mut self, direction: Direction, batch: &Batch, batch_state: &BatchState) {
        let outstanding = batch.core_quantity.saturating_sub(batch_state.filled);
        if outstanding.is_zero() {
            return;
        }
        let unit = Unit::Batch(direction, batch.index);
        let limit_price = self.entry_limit(direction, batch.price);
        let demand = Demand {
            unit,
            purpose: OrderPurpose::Core,
            side: direction.entry_side(),
            limit_price,
            quantity: outstanding,
            reduce_only: false,
            blocked: self.blocked(unit, OrderPurpose::Core),
        };
        match self.reconcile(&demand) {
            Placement::Blocked => self.waiting_for_fills = true,
            // 剩余量按批次价足够、只是因为避让才不够：本批不结束，主仓建仓单暂不挂出，
            // 等自身反向订单成交或价格回到批次价后再补齐（第 4.2.3 节）。
            Placement::BelowMinimum if limit_price != batch.price => {
                self.plan.waiting.push(WaitReason::LimitAdjustedByOwnOrder);
            }
            _ => {}
        }
    }

    /// 本批已经结束：不再挂主仓建仓单，在场的那张也要撤掉。
    ///
    /// 结束可能是迟到成交带来的，而这张单是上一次规划按当时的 `C − F` 挂出来的；
    /// 留着它就会在本批已经决定不再建仓之后又建进来。
    fn cancel_core_orders(&mut self, unit: Unit) {
        self.plan.cancel.extend(
            self.working_orders
                .iter()
                .filter(|order| order.unit == unit && order.purpose == OrderPurpose::Core)
                .map(|order| order.client_order_id.clone()),
        );
    }

    /// 一侧的格子：启用的照常循环，没启用的等所属批次。
    fn plan_cells(&mut self, side: &SidePlan) {
        let direction = side.direction();
        for cell in side.grid().cells() {
            let Some(cell_state) = self.cell_state(direction, cell.index) else {
                continue;
            };
            if !cell_state.enabled {
                // 所属批次已经结束的格子不会再启用，不算在等批次。
                let batch_running = cell
                    .batch
                    .and_then(|batch| self.batch_state(direction, batch))
                    .is_some_and(|batch_state| !batch_state.finished);
                self.waiting_for_batch |= batch_running;
                continue;
            }
            self.plan_cell(direction, cell, cell_state);
        }
    }

    /// 一个启用格子的两侧：先建仓腿、再平仓腿。
    fn plan_cell(&mut self, direction: Direction, cell: &Cell, cell_state: &CellState) {
        let held = cell_state.held(cell.quantity);
        let unit = Unit::Cell(direction, cell.index);
        // 没有主仓时格子自成单位（第 4.1 节）：还没放行的格子不挂第一次建仓单。
        // 已经持有仓位的格子一定是放行过的，它的买卖循环不受限速约束（第 4.3 节）。
        if self.strategy.is_released(self.state, unit) {
            self.plan_cell_entry(direction, cell, cell_state, unit);
        } else {
            self.waiting_for_throttle = true;
        }
        let exit = Demand {
            unit,
            purpose: OrderPurpose::GridExit,
            side: direction.exit_side(),
            // 平仓单的限价要用建仓腿之后的最新边界：建仓腿的限价可能刚被调整过。
            limit_price: self.exit_limit(direction, cell.exit_price),
            quantity: held,
            // 中性网格的平仓单不标只减仓：一侧的平仓在交易所层面可能是在减另一侧的仓位
            // （第 5.2 节）。
            reduce_only: !self.strategy.is_neutral(),
            blocked: self.blocked(unit, OrderPurpose::GridExit),
        };
        if let Placement::Blocked = self.reconcile(&exit) {
            self.waiting_for_fills = true;
        }
    }

    /// 格子的建仓单：待建仓的数量挂在自己的建仓价上。
    fn plan_cell_entry(
        &mut self,
        direction: Direction,
        cell: &Cell,
        cell_state: &CellState,
        unit: Unit,
    ) {
        let entry = Demand {
            unit,
            purpose: OrderPurpose::GridEntry,
            side: direction.entry_side(),
            limit_price: self.entry_limit(direction, cell.entry_price),
            quantity: cell_state.to_buy_back,
            reduce_only: false,
            blocked: self.blocked(unit, OrderPurpose::GridEntry),
        };
        if let Placement::Blocked = self.reconcile(&entry) {
            self.waiting_for_fills = true;
        }
    }

    /// 一侧某一批的状态。
    fn batch_state(&self, direction: Direction, index: BatchIndex) -> Option<&'a BatchState> {
        self.state.side(direction)?.batch(index)
    }

    /// 一侧某一个格子的状态。
    fn cell_state(
        &self,
        direction: Direction,
        index: crate::grid::CellIndex,
    ) -> Option<&'a CellState> {
        self.state.side(direction)?.cell(index)
    }

    /// 建仓腿的限价：买单不能高过自身最低卖价，卖单不能低过自身最高买价（第 6 节）。
    fn entry_limit(&self, direction: Direction, desired: Price) -> Price {
        self.leg_limit(direction.entry_side(), desired)
    }

    /// 平仓腿的限价。
    fn exit_limit(&self, direction: Direction, desired: Price) -> Price {
        self.leg_limit(direction.exit_side(), desired)
    }

    fn leg_limit(&self, side: Side, desired: Price) -> Price {
        match side {
            Side::Buy => self.bounds.buy_limit(desired, self.rule),
            Side::Sell => self.bounds.sell_limit(desired, self.rule),
        }
    }

    /// 汇总等待原因。顺序固定：限速放行、所属批次、成交对账。
    fn finish(mut self) -> Plan {
        if self.waiting_for_throttle {
            self.plan.waiting.push(WaitReason::WaitingForThrottle);
        }
        if self.waiting_for_batch {
            self.plan.waiting.push(WaitReason::WaitingForBatch);
        }
        if self.waiting_for_fills {
            self.plan.waiting.push(WaitReason::FillsPending);
        }
        self.plan
    }

    /// 这个单位的这一侧是不是还没对完账。
    fn blocked(&self, unit: Unit, purpose: OrderPurpose) -> bool {
        self.pending.contains(&PendingUnit { unit, purpose })
    }

    /// 让某一侧的在场订单与应当在场的数量一致（见 [`Strategy::reconcile`]）。
    fn reconcile(&mut self, demand: &Demand) -> Placement {
        self.strategy.reconcile(
            self.working_orders,
            demand,
            &mut self.plan,
            &mut self.bounds,
        )
    }
}

impl Strategy {
    /// 规划下一步动作。纯计算，不读写任何外部状态。
    ///
    /// `pending` 是还没有对完账的单位：这些单位的这一侧只保留预留，不下新单。
    /// 一条统一规则就够——撤单期间的部分成交、恢复时还没到的流水、迟到的流水，
    /// 都会让订单停留在「没对完账」上，因而不会按旧数量补单。
    ///
    /// `market_price` 是当前标记价。挂单都挂在预设价格上，是否立即成交由交易所决定；
    /// 它记在 [`Plan`] 里，供预览和日志使用。
    pub fn plan(
        &self,
        state: &StrategyState,
        working_orders: &[WorkingOrder],
        pending: &[PendingUnit],
        market_price: Price,
    ) -> Plan {
        let mut planner = Planner::new(self, state, working_orders, pending, market_price);
        planner.plan_sides();
        planner.finish()
    }

    /// 让某一侧的在场订单与应当在场的数量一致：价格或数量不符的撤掉，缺的补上。
    ///
    /// 成交还没对完账的那一侧只做前半段：在场订单该撤的照撤（撤单不产生新风险），
    /// 但不补新单——撤单前的最终成交量还不知道，按旧数量补一张就会多买或多卖。
    fn reconcile(
        &self,
        working_orders: &[WorkingOrder],
        demand: &Demand,
        plan: &mut Plan,
        bounds: &mut OwnOrderBounds,
    ) -> Placement {
        let mut kept = Quantity::ZERO;
        for order in working_orders
            .iter()
            .filter(|order| order.unit == demand.unit && order.purpose == demand.purpose)
        {
            if order.limit_price == demand.limit_price && kept + order.remaining <= demand.quantity
            {
                kept = kept + order.remaining;
            } else {
                plan.cancel.push(order.client_order_id.clone());
            }
        }

        let missing = demand.quantity.saturating_sub(kept);
        if missing.is_zero() {
            return Placement::Handled;
        }
        // 成交还没对完账：保留预留，不补新单。
        if demand.blocked {
            return Placement::Blocked;
        }
        // 剩余量不足最低订单金额时这一侧先不挂单（第 4.4 节）。
        if !self.spec().meets_minimum_order(missing, demand.limit_price) {
            return Placement::BelowMinimum;
        }
        plan.place.push(OrderIntent {
            unit: demand.unit,
            purpose: demand.purpose,
            side: demand.side,
            limit_price: demand.limit_price,
            quantity: missing,
            reduce_only: demand.reduce_only,
        });
        bounds.include(demand.side, demand.limit_price);
        Placement::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::{OrderIntent, PendingUnit, Plan, WaitReason};
    use crate::Strategy;
    use crate::batch::BatchIndex;
    use crate::fixtures::{
        at, core_long_strategy, fine_core_long_strategy, five_cell_strategy, short_grid_strategy,
        single_batch_core_strategy, single_small_batch_strategy, throttled_core_long_strategy,
    };
    use crate::grid::CellIndex;
    use crate::order::{
        ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Side, Unit, WorkingOrder,
    };
    use crate::side::Direction;
    use crate::state::{SideState, StrategyState};
    use crate::units::{Price, Quantity};

    /// 测试里大多只看多头侧。
    fn long_state(state: &StrategyState) -> &SideState {
        state
            .side(Direction::Long)
            .expect("the long side is configured")
    }

    fn long_state_mut(state: &mut StrategyState) -> &mut SideState {
        state
            .side_mut(Direction::Long)
            .expect("the long side is configured")
    }

    fn working_order(
        id: &str,
        unit: Unit,
        purpose: OrderPurpose,
        price: &str,
        remaining: &str,
    ) -> WorkingOrder {
        WorkingOrder {
            client_order_id: ClientOrderId::try_new(id).unwrap(),
            unit,
            purpose,
            limit_price: Price::parse(price).unwrap(),
            remaining: Quantity::parse(remaining).unwrap(),
        }
    }

    fn order_from(intent: &OrderIntent, id: &str) -> OrderRecord {
        OrderRecord {
            client_order_id: ClientOrderId::try_new(id).unwrap(),
            unit: intent.unit,
            purpose: intent.purpose,
            limit_price: intent.limit_price,
            quantity: intent.quantity,
            cumulative_filled: Quantity::ZERO,
            reduce_only: intent.reduce_only,
            status: OrderStatus::Open,
        }
    }

    fn intent(plan: &Plan, unit: Unit, purpose: OrderPurpose) -> &OrderIntent {
        plan.place
            .iter()
            .find(|intent| intent.unit == unit && intent.purpose == purpose)
            .unwrap_or_else(|| panic!("no {purpose:?} order for {unit}"))
    }

    fn has_intent(plan: &Plan, unit: Unit, purpose: OrderPurpose) -> bool {
        plan.place
            .iter()
            .any(|intent| intent.unit == unit && intent.purpose == purpose)
    }

    /// 第 0 格：买价 95000，卖价 96000，数量 .015；主仓成交 .07 后启用 95、94 两格。
    fn two_cells_enabled(strategy: &Strategy) -> StrategyState {
        let mut state = strategy.initial_state();
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.07").unwrap();
        strategy.advance(&mut state, &[]);
        state
    }

    #[test]
    fn b04_batches_buy_at_their_batch_price_and_disabled_cells_wait() {
        let strategy = core_long_strategy();
        let state = two_cells_enabled(&strategy);

        let plan = strategy.plan(&state, &[], &[], Price::parse("96000").unwrap());

        // 每批的主仓买单挂在自己的批次价上，数量是 C − F。
        let prices = ["95000", "90000", "85000", "80000"];
        for (index, price) in prices.iter().enumerate() {
            let intent = intent(
                &plan,
                Unit::Batch(Direction::Long, BatchIndex(index)),
                OrderPurpose::Core,
            );
            assert_eq!(intent.limit_price, Price::parse(price).unwrap());
            let expected = if index == 0 { "0.105" } else { "0.175" };
            assert_eq!(intent.quantity, Quantity::parse(expected).unwrap());
        }
        // 已启用的两格挂买回单，其余格子等所属批次。
        assert_eq!(
            intent(
                &plan,
                Unit::Cell(Direction::Long, CellIndex(0)),
                OrderPurpose::GridEntry
            )
            .quantity,
            Quantity::parse("0.015").unwrap()
        );
        // 主仓只买不卖：批次上的订单全是买单。
        assert!(
            plan.place
                .iter()
                .filter(|intent| matches!(intent.unit, Unit::Batch(..)))
                .all(|intent| intent.side == Side::Buy && !intent.reduce_only)
        );
        assert!(!has_intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(2)),
            OrderPurpose::GridEntry
        ));
        assert!(plan.waiting.contains(&WaitReason::WaitingForBatch));
    }

    /// 做空网格是多头网格的镜像：每格在自己的开空价挂卖单，还没持有就没有买平单。
    #[test]
    fn b10_the_short_grid_places_the_mirror_orders() {
        let strategy = short_grid_strategy();
        let state = strategy.initial_state();

        let plan = strategy.plan(&state, &[], &[], Price::parse("94000").unwrap());

        // 二十格各挂一张卖单，价格从 75000 往上每格 1000，数量 .05。
        assert_eq!(plan.place.len(), 20);
        for (index, intent) in plan.place.iter().enumerate() {
            assert_eq!(intent.unit, Unit::Cell(Direction::Short, CellIndex(index)));
            assert_eq!(intent.purpose, OrderPurpose::GridEntry);
            assert_eq!(intent.side, Side::Sell);
            assert_eq!(
                intent.limit_price,
                Price::parse(&(75000 + index * 1000).to_string()).unwrap()
            );
            assert_eq!(intent.quantity, Quantity::parse("0.05").unwrap());
            assert!(!intent.reduce_only);
        }
        // 还没有持有任何一格，所以没有买平单。
        assert!(
            plan.place
                .iter()
                .all(|intent| intent.purpose != OrderPurpose::GridExit)
        );
        assert!(plan.cancel.is_empty());
    }

    #[test]
    fn b04_enabled_cells_sell_only_what_they_hold() {
        let strategy = core_long_strategy();
        let mut state = two_cells_enabled(&strategy);
        // 第 0 格买回 .005，持有 .01。
        long_state_mut(&mut state)
            .cell_mut(CellIndex(0))
            .unwrap()
            .to_buy_back = Quantity::parse("0.01").unwrap();

        let plan = strategy.plan(&state, &[], &[], Price::parse("95000").unwrap());

        assert_eq!(
            intent(
                &plan,
                Unit::Cell(Direction::Long, CellIndex(0)),
                OrderPurpose::GridEntry
            )
            .quantity,
            Quantity::parse("0.01").unwrap()
        );
        let sell = intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridExit,
        );
        assert_eq!(sell.limit_price, Price::parse("96000").unwrap());
        assert_eq!(sell.quantity, Quantity::parse("0.005").unwrap());
        assert!(sell.reduce_only);
    }

    #[test]
    fn b05c_a_batch_waits_when_only_the_adjusted_limit_is_below_the_minimum() {
        let strategy = single_batch_core_strategy();
        let mut state = strategy.initial_state();
        // 主仓成交 6.9，还差 .1；两格已启用并全部持有。
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("6.9").unwrap();
        strategy.advance(&mut state, &[]);
        for index in 0..2 {
            long_state_mut(&mut state)
                .cell_mut(CellIndex(index))
                .unwrap()
                .to_buy_back = Quantity::ZERO;
        }
        assert!(
            long_state(&state).cells()[..2]
                .iter()
                .all(|cell| cell.enabled)
        );
        assert!(!long_state(&state).cells()[2].enabled);

        // 自身卖单在 100，主仓买单只能挂 99.99，.1 × 99.99 = 9.999 美元。
        let working = vec![
            working_order(
                "0x01",
                Unit::Cell(Direction::Long, CellIndex(0)),
                OrderPurpose::GridExit,
                "101",
                "1",
            ),
            working_order(
                "0x02",
                Unit::Cell(Direction::Long, CellIndex(1)),
                OrderPurpose::GridExit,
                "100",
                "1",
            ),
        ];
        let plan = strategy.plan(&state, &working, &[], Price::parse("100").unwrap());

        // 按批次价 100 计算是 10 美元，够最低金额：本批不结束，只是暂时挂不出去。
        assert!(!long_state(&state).batch(BatchIndex(0)).unwrap().finished);
        assert!(!has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core
        ));
        assert!(plan.waiting.contains(&WaitReason::LimitAdjustedByOwnOrder));
        assert!(plan.cancel.is_empty());

        // 卖 100 成交后不再有交叉，主仓在批次价上补齐。
        let mut after_sell = state.clone();
        let mut sell = OrderRecord {
            client_order_id: ClientOrderId::try_new("0x02").unwrap(),
            unit: Unit::Cell(Direction::Long, CellIndex(1)),
            purpose: OrderPurpose::GridExit,
            limit_price: Price::parse("100").unwrap(),
            quantity: Quantity::parse("1").unwrap(),
            cumulative_filled: Quantity::ZERO,
            reduce_only: true,
            status: OrderStatus::Open,
        };
        strategy.apply_fill(
            &mut after_sell,
            &mut sell,
            Quantity::parse("1").unwrap(),
            at(0),
        );
        let working = vec![working_order(
            "0x01",
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridExit,
            "101",
            "1",
        )];
        let plan = strategy.plan(&after_sell, &working, &[], Price::parse("100").unwrap());

        let core = intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core,
        );
        assert_eq!(core.limit_price, Price::parse("100").unwrap());
        assert_eq!(core.quantity, Quantity::parse("0.1").unwrap());
        assert!(!plan.waiting.contains(&WaitReason::LimitAdjustedByOwnOrder));

        // 主仓买完后本批结束，98 格随后启用。
        long_state_mut(&mut after_sell)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("7").unwrap();
        strategy.advance(&mut after_sell, &[]);
        assert!(
            long_state(&after_sell)
                .batch(BatchIndex(0))
                .unwrap()
                .finished
        );
        assert!(
            long_state(&after_sell)
                .cells()
                .iter()
                .all(|cell| cell.enabled)
        );
    }

    #[test]
    fn b08_a_side_below_the_minimum_is_left_out_until_the_cell_is_whole_again() {
        let strategy = fine_core_long_strategy();
        let mut state = strategy.initial_state();
        // 第 0 格卖出 .0149，只剩 .0001 持有：.0001 × 96000 = 9.6 美元，不足最低金额。
        long_state_mut(&mut state)
            .cell_mut(CellIndex(0))
            .unwrap()
            .enabled = true;
        long_state_mut(&mut state)
            .cell_mut(CellIndex(0))
            .unwrap()
            .to_buy_back = Quantity::parse("0.0149").unwrap();

        let plan = strategy.plan(&state, &[], &[], Price::parse("95000").unwrap());

        assert!(!has_intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridExit
        ));
        let buy = intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridEntry,
        );
        assert_eq!(buy.quantity, Quantity::parse("0.0149").unwrap());

        // 买回全部成交后格子恢复整份，按 .015 挂卖。
        let mut buy = order_from(buy, "0x01");
        strategy.apply_fill(
            &mut state,
            &mut buy,
            Quantity::parse("0.0149").unwrap(),
            at(0),
        );
        let plan = strategy.plan(&state, &[], &[], Price::parse("95000").unwrap());

        assert!(!has_intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridEntry
        ));
        let sell = intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridExit,
        );
        assert_eq!(sell.quantity, Quantity::parse("0.015").unwrap());
        assert_eq!(sell.limit_price, Price::parse("96000").unwrap());
    }

    #[test]
    fn b13_a_new_limit_never_crosses_this_strategys_own_opposite_order() {
        let strategy = five_cell_strategy();
        let mut state = strategy.initial_state();
        // 价格跌到 91500：95、94、93、91 四格已买入并持有，92 格没成交。
        for index in [0, 1, 2, 4] {
            let cell = long_state_mut(&mut state)
                .cell_mut(CellIndex(index))
                .unwrap();
            cell.enabled = true;
            cell.to_buy_back = Quantity::ZERO;
        }
        let working = vec![
            working_order(
                "0x01",
                Unit::Cell(Direction::Long, CellIndex(0)),
                OrderPurpose::GridExit,
                "96000",
                "0.05",
            ),
            working_order(
                "0x02",
                Unit::Cell(Direction::Long, CellIndex(1)),
                OrderPurpose::GridExit,
                "95000",
                "0.05",
            ),
            working_order(
                "0x03",
                Unit::Cell(Direction::Long, CellIndex(2)),
                OrderPurpose::GridExit,
                "94000",
                "0.05",
            ),
            working_order(
                "0x04",
                Unit::Cell(Direction::Long, CellIndex(4)),
                OrderPurpose::GridExit,
                "92000",
                "0.05",
            ),
            working_order(
                "0x05",
                Unit::Cell(Direction::Long, CellIndex(3)),
                OrderPurpose::GridEntry,
                "92000",
                "0.05",
            ),
        ];

        let plan = strategy.plan(&state, &working, &[], Price::parse("91500").unwrap());

        // 92 格的买单下调到自身最低卖价减一个报价单位，91 格的卖单上移一个报价单位。
        let buy = intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(3)),
            OrderPurpose::GridEntry,
        );
        assert_eq!(buy.limit_price, Price::parse("91999").unwrap());
        let sell = intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(4)),
            OrderPurpose::GridExit,
        );
        assert_eq!(sell.limit_price, Price::parse("92001").unwrap());
        // 不相交，两张单不会互相成交。
        assert!(buy.limit_price < sell.limit_price);
        // 在场订单价格变了，先撤掉再挂。
        assert_eq!(plan.cancel.len(), 2);
        // 高处的卖单不受影响。
        assert!(!has_intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridExit
        ));
    }

    #[test]
    fn b13_limits_stay_at_the_preset_prices_when_nothing_crosses() {
        let strategy = five_cell_strategy();
        let mut state = strategy.initial_state();
        // 高处的三格持有等卖出，92 格卖完了等买回：买价 92000 低于最低卖价 94000，
        // 不相交，各自的限价都不动。
        for (index, to_buy_back) in [(0, "0"), (1, "0"), (2, "0"), (3, "0.05")] {
            let cell = long_state_mut(&mut state)
                .cell_mut(CellIndex(index))
                .unwrap();
            cell.enabled = true;
            cell.to_buy_back = Quantity::parse(to_buy_back).unwrap();
        }

        let plan = strategy.plan(&state, &[], &[], Price::parse("91500").unwrap());

        assert_eq!(
            intent(
                &plan,
                Unit::Cell(Direction::Long, CellIndex(3)),
                OrderPurpose::GridEntry
            )
            .limit_price,
            Price::parse("92000").unwrap()
        );
        for (index, price) in [(0, "96000"), (1, "95000"), (2, "94000")] {
            assert_eq!(
                intent(
                    &plan,
                    Unit::Cell(Direction::Long, CellIndex(index)),
                    OrderPurpose::GridExit
                )
                .limit_price,
                Price::parse(price).unwrap()
            );
        }
    }

    #[test]
    fn b13_a_buy_and_a_sell_planned_in_the_same_step_do_not_cross() {
        let strategy = five_cell_strategy();
        let mut state = strategy.initial_state();
        // 高处的四格持有等卖出，92 格卖完了等买回：92 格的买价和 91 格的卖价都是
        // 92000。两张单都是这一步新挂出去的，同样不能相交。
        for index in [0, 1, 2, 4] {
            long_state_mut(&mut state)
                .cell_mut(CellIndex(index))
                .unwrap()
                .to_buy_back = Quantity::ZERO;
        }

        let plan = strategy.plan(&state, &[], &[], Price::parse("91500").unwrap());

        let buy = intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(3)),
            OrderPurpose::GridEntry,
        );
        assert_eq!(buy.limit_price, Price::parse("92000").unwrap());
        let sell = intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(4)),
            OrderPurpose::GridExit,
        );
        assert_eq!(sell.limit_price, Price::parse("92001").unwrap());
        assert!(buy.limit_price < sell.limit_price);

        // 下一步两张单都还在场，价格不再变化。
        let working = vec![
            working_order(
                "0x01",
                Unit::Cell(Direction::Long, CellIndex(0)),
                OrderPurpose::GridExit,
                "96000",
                "0.05",
            ),
            working_order(
                "0x02",
                Unit::Cell(Direction::Long, CellIndex(1)),
                OrderPurpose::GridExit,
                "95000",
                "0.05",
            ),
            working_order(
                "0x03",
                Unit::Cell(Direction::Long, CellIndex(2)),
                OrderPurpose::GridExit,
                "94000",
                "0.05",
            ),
            working_order(
                "0x04",
                Unit::Cell(Direction::Long, CellIndex(3)),
                OrderPurpose::GridEntry,
                "92000",
                "0.05",
            ),
            working_order(
                "0x05",
                Unit::Cell(Direction::Long, CellIndex(4)),
                OrderPurpose::GridExit,
                "92001",
                "0.05",
            ),
        ];
        let plan = strategy.plan(&state, &working, &[], Price::parse("91500").unwrap());
        assert!(plan.cancel.is_empty());
        assert!(plan.place.is_empty());
    }

    #[test]
    fn a_cell_whose_batch_ended_early_is_not_waiting_for_its_batch() {
        // 一批一格：主仓成交 .02 后本批提前结束，这一格不会再启用。
        let strategy = single_small_batch_strategy();
        let mut state = strategy.initial_state();
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.02").unwrap();
        strategy.advance(&mut state, &[]);
        assert!(long_state(&state).batch(BatchIndex(0)).unwrap().finished);
        assert!(!long_state(&state).cells()[0].enabled);

        let plan = strategy.plan(&state, &[], &[], Price::parse("99").unwrap());

        // 本批已经结束，不会再为它下任何单，也就不是「等所属批次」。
        assert!(!plan.waiting.contains(&WaitReason::WaitingForBatch));
        assert!(plan.place.is_empty());
    }

    /// 本批结束后，上一次规划留下的主仓买单必须撤掉。
    ///
    /// 结束可能是迟到成交带来的：那张单是上一次按当时的 `C − F` 挂出来的，
    /// 留着它就会在本批已经决定不再买入之后又买进来。
    #[test]
    fn a_finished_batch_cancels_its_working_core_order() {
        let strategy = core_long_strategy();
        let mut state = two_cells_enabled(&strategy);
        // 迟到成交把主仓补到 C：本批结束。
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.175").unwrap();
        strategy.advance(&mut state, &[]);
        assert!(long_state(&state).batch(BatchIndex(0)).unwrap().finished);
        // 上一次规划按 `C − F = .105` 挂出去的那张单还在场。
        let working = vec![working_order(
            "0x01",
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core,
            "95000",
            "0.105",
        )];

        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());

        assert_eq!(
            plan.cancel,
            vec![ClientOrderId::try_new("0x01").unwrap()],
            "the core order of a finished batch must be canceled"
        );
        assert!(!has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core
        ));
        // 其他批次不受影响。
        assert!(has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(1)),
            OrderPurpose::Core
        ));
    }

    /// 已经结束的批次不受放行影响：还没放行的批次也要撤掉在场的主仓买单。
    ///
    /// 放行进度可能落后于批次的真实进度（运行中改成限速、或者旧版本写下的状态），
    /// 而结束的批次不会再成交：等它放行可能永远等不到，那张单就会一直留在场上买。
    #[test]
    fn a_finished_batch_that_is_not_released_still_cancels_its_core_order() {
        let strategy = throttled_core_long_strategy();
        let mut state = strategy.initial_state();
        // 第 1 批成交结束，但放行进度只到第 0 批。
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(1))
            .unwrap()
            .filled = Quantity::parse("0.175").unwrap();
        strategy.advance(&mut state, &[]);
        assert!(long_state(&state).batch(BatchIndex(1)).unwrap().finished);
        assert!(!strategy.is_released(&state, Unit::Batch(Direction::Long, BatchIndex(1))));

        let working = vec![working_order(
            "0x01",
            Unit::Batch(Direction::Long, BatchIndex(1)),
            OrderPurpose::Core,
            "90000",
            "0.105",
        )];

        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());

        assert_eq!(
            plan.cancel,
            vec![ClientOrderId::try_new("0x01").unwrap()],
            "a finished batch must cancel its core order even when it is not released"
        );
        assert!(!has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(1)),
            OrderPurpose::Core
        ));
    }

    #[test]
    fn working_orders_that_already_match_are_kept() {
        let strategy = core_long_strategy();
        let state = two_cells_enabled(&strategy);
        let working = vec![
            working_order(
                "0x01",
                Unit::Batch(Direction::Long, BatchIndex(0)),
                OrderPurpose::Core,
                "95000",
                "0.105",
            ),
            working_order(
                "0x02",
                Unit::Cell(Direction::Long, CellIndex(0)),
                OrderPurpose::GridEntry,
                "95000",
                "0.015",
            ),
        ];

        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());

        assert!(plan.cancel.is_empty());
        assert!(!has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core
        ));
        assert!(!has_intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridEntry
        ));
        // 其余批次和格子照常挂出。
        assert!(has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(1)),
            OrderPurpose::Core
        ));
        assert!(has_intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(1)),
            OrderPurpose::GridEntry
        ));
    }

    #[test]
    fn working_orders_with_a_stale_price_or_size_are_canceled() {
        let strategy = core_long_strategy();
        let state = two_cells_enabled(&strategy);
        let working = vec![
            working_order(
                "0x01",
                Unit::Batch(Direction::Long, BatchIndex(0)),
                OrderPurpose::Core,
                "95000",
                "0.2",
            ),
            working_order(
                "0x02",
                Unit::Cell(Direction::Long, CellIndex(0)),
                OrderPurpose::GridEntry,
                "94000",
                "0.015",
            ),
        ];

        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());

        let mut canceled: Vec<&str> = plan.cancel.iter().map(|id| id.as_str()).collect();
        canceled.sort_unstable();
        assert_eq!(canceled, vec!["0x01", "0x02"]);
        assert_eq!(
            intent(
                &plan,
                Unit::Batch(Direction::Long, BatchIndex(0)),
                OrderPurpose::Core
            )
            .quantity,
            Quantity::parse("0.105").unwrap()
        );
    }

    #[test]
    fn a_working_core_buy_reserves_its_quantity() {
        let strategy = core_long_strategy();
        let mut state = two_cells_enabled(&strategy);
        // 本批还差 .105，其中 .1 已经挂在场上了。
        let working = vec![working_order(
            "0x01",
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core,
            "95000",
            "0.1",
        )];

        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());

        assert_eq!(
            intent(
                &plan,
                Unit::Batch(Direction::Long, BatchIndex(0)),
                OrderPurpose::Core
            )
            .quantity,
            Quantity::parse("0.005").unwrap()
        );

        // 在场订单已经覆盖本批还差的部分：不重复挂单。
        long_state_mut(&mut state)
            .batch_mut(BatchIndex(0))
            .unwrap()
            .filled = Quantity::parse("0.075").unwrap();
        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());
        assert!(!has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core
        ));
        assert!(plan.cancel.is_empty());
    }

    /// 成交还没对完账的单位：在场订单照常撤，但不补新单。
    ///
    /// 撤单前的最终成交量还不知道，按旧数量补一张就会多买或多卖；同一单位同一用途
    /// 之外的规划不受影响。
    #[test]
    fn a_pending_unit_cancels_but_does_not_place() {
        let strategy = core_long_strategy();
        let state = two_cells_enabled(&strategy);
        let working = vec![working_order(
            "0x01",
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core,
            "95000",
            "0.2",
        )];
        let pending = [PendingUnit {
            unit: Unit::Batch(Direction::Long, BatchIndex(0)),
            purpose: OrderPurpose::Core,
        }];

        let plan = strategy.plan(&state, &working, &pending, Price::parse("96000").unwrap());

        // 在场订单的数量对不上，照常撤掉。
        assert_eq!(plan.cancel, vec![ClientOrderId::try_new("0x01").unwrap()]);
        // 但这一批的主仓买单不补：等对完账再按新的状态算数量。
        assert!(!has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(0)),
            OrderPurpose::Core
        ));
        assert!(plan.waiting.contains(&WaitReason::FillsPending));
        // 其他批次和其他用途不受影响。
        assert!(has_intent(
            &plan,
            Unit::Batch(Direction::Long, BatchIndex(1)),
            OrderPurpose::Core
        ));
        assert!(has_intent(
            &plan,
            Unit::Cell(Direction::Long, CellIndex(0)),
            OrderPurpose::GridEntry
        ));
    }
}
