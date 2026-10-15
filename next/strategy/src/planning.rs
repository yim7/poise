//! 规划：给定状态、在场订单和当前价格，输出要下的单、要撤的单和等待原因。
//!
//! 规则（做多侧、不限速）：
//! - 每批主仓在自己的批次价挂买单，不追高；成交后格子按比例启用（第 4.2 节）；
//! - 启用后的格子：持有部分在卖价挂卖单（只减仓），待买回部分在买价挂买单；
//! - 某一侧剩余量不足最低订单金额时，这一侧先不挂单，只挂另一侧（第 4.4 节）；
//! - 新订单的限价不与本策略在场的反向订单相交（第 6 节）。

use serde::{Deserialize, Serialize};

use crate::Strategy;
use crate::order::{ClientOrderId, OrderPurpose, Side, Unit, WorkingOrder};
use crate::state::StrategyState;
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
/// 除最后两条外是设计第 11 节列出的集合。本任务支持的配置（做多、不限速）会产生
/// 「等待所属批次」和「限价因自身反向订单下调」两种；其余属于后续任务。
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
    /// 行情过期。
    StaleMarketData,
    /// 成交推送断开。
    FillStreamDisconnected,
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
#[derive(Debug, Default)]
struct OwnOrderBounds {
    lowest_sell: Option<Price>,
    highest_buy: Option<Price>,
}

impl OwnOrderBounds {
    fn of(working_orders: &[WorkingOrder]) -> Self {
        let mut bounds = Self::default();
        for order in working_orders {
            bounds.include(order.purpose.side(), order.limit_price);
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

    fn buy_limit(&self, desired: Price, tick: Price) -> Price {
        match self.lowest_sell {
            Some(lowest_sell) => desired.min(lowest_sell.saturating_sub(tick)),
            None => desired,
        }
    }

    fn sell_limit(&self, desired: Price, tick: Price) -> Price {
        match self.highest_buy {
            Some(highest_buy) => desired.max(highest_buy.saturating_add(tick)),
            None => desired,
        }
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
        let mut plan = Plan {
            market_price,
            ..Plan::default()
        };
        let mut bounds = OwnOrderBounds::of(working_orders);
        let tick = self.spec().price_tick();
        let blocked =
            |unit: Unit, purpose: OrderPurpose| pending.contains(&PendingUnit { unit, purpose });
        let mut waiting_for_fills = false;

        // 主仓：每批在自己的批次价挂买单，不追高。
        for batch in self.batches().batches() {
            let Some(batch_state) = state.batch(batch.index) else {
                continue;
            };
            if batch_state.finished {
                // 本批已经结束：不再挂主仓买单，在场的那张也要撤掉。
                // 结束可能是迟到成交带来的，而这张单是上一次规划按当时的 `C − F` 挂出来的；
                // 留着它就会在本批已经决定不再买入之后又买进来。
                plan.cancel.extend(
                    working_orders
                        .iter()
                        .filter(|order| {
                            order.unit == Unit::Batch(batch.index)
                                && order.purpose == OrderPurpose::CoreBuy
                        })
                        .map(|order| order.client_order_id.clone()),
                );
                continue;
            }
            // 本批最多能在场的数量是 `C − F`；在场订单已经有多少由 reconcile 判断。
            let outstanding = batch.core_quantity.saturating_sub(batch_state.filled);
            if outstanding.is_zero() {
                continue;
            }
            let unit = Unit::Batch(batch.index);
            let limit_price = bounds.buy_limit(batch.price, tick);
            let demand = Demand {
                unit,
                purpose: OrderPurpose::CoreBuy,
                limit_price,
                quantity: outstanding,
                reduce_only: false,
                blocked: blocked(unit, OrderPurpose::CoreBuy),
            };
            match self.reconcile(working_orders, &demand, &mut plan, &mut bounds) {
                Placement::Blocked => waiting_for_fills = true,
                // 剩余量按批次价足够、只是因为下调才不够：本批不结束，主仓买单暂不挂出，
                // 等自身卖单成交或价格回到批次价后再补齐（第 4.2.3 节）。
                Placement::BelowMinimum if limit_price != batch.price => {
                    plan.waiting.push(WaitReason::LimitAdjustedByOwnOrder);
                }
                _ => {}
            }
        }

        // 格子：启用的照常循环，没启用的等所属批次。
        let mut waiting_for_batch = false;
        for cell in self.grid().cells() {
            let Some(cell_state) = state.cell(cell.index) else {
                continue;
            };
            if !cell_state.enabled {
                // 所属批次已经结束的格子不会再启用，不算在等批次。
                let batch_running = cell
                    .batch
                    .and_then(|batch| state.batch(batch))
                    .is_some_and(|batch_state| !batch_state.finished);
                waiting_for_batch |= batch_running;
                continue;
            }
            let held = cell_state.held(cell.quantity);
            let unit = Unit::Cell(cell.index);
            // 先买回、再卖出：卖出看到的边界包含刚规划出去的买回单。
            let buy = Demand {
                unit,
                purpose: OrderPurpose::GridBuy,
                limit_price: bounds.buy_limit(cell.buy_price, tick),
                quantity: cell_state.to_buy_back,
                reduce_only: false,
                blocked: blocked(unit, OrderPurpose::GridBuy),
            };
            if let Placement::Blocked = self.reconcile(working_orders, &buy, &mut plan, &mut bounds)
            {
                waiting_for_fills = true;
            }
            let sell = Demand {
                unit,
                purpose: OrderPurpose::GridSell,
                // 卖单的限价要用买回之后的最新边界：买回的限价可能刚被下调。
                limit_price: bounds.sell_limit(cell.sell_price, tick),
                quantity: held,
                reduce_only: true,
                blocked: blocked(unit, OrderPurpose::GridSell),
            };
            if let Placement::Blocked =
                self.reconcile(working_orders, &sell, &mut plan, &mut bounds)
            {
                waiting_for_fills = true;
            }
        }
        if waiting_for_batch {
            plan.waiting.push(WaitReason::WaitingForBatch);
        }
        if waiting_for_fills {
            plan.waiting.push(WaitReason::FillsPending);
        }
        plan
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
            side: demand.purpose.side(),
            limit_price: demand.limit_price,
            quantity: missing,
            reduce_only: demand.reduce_only,
        });
        bounds.include(demand.purpose.side(), demand.limit_price);
        Placement::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::{OrderIntent, PendingUnit, Plan, WaitReason};
    use crate::Strategy;
    use crate::batch::BatchIndex;
    use crate::fixtures::{
        core_long_strategy, fine_core_long_strategy, five_cell_strategy,
        single_batch_core_strategy, single_small_batch_strategy,
    };
    use crate::grid::CellIndex;
    use crate::order::{
        ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Side, Unit, WorkingOrder,
    };
    use crate::state::StrategyState;
    use crate::units::{Price, Quantity};

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
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.07").unwrap();
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
            let intent = intent(&plan, Unit::Batch(BatchIndex(index)), OrderPurpose::CoreBuy);
            assert_eq!(intent.limit_price, Price::parse(price).unwrap());
            let expected = if index == 0 { "0.105" } else { "0.175" };
            assert_eq!(intent.quantity, Quantity::parse(expected).unwrap());
        }
        // 已启用的两格挂买回单，其余格子等所属批次。
        assert_eq!(
            intent(&plan, Unit::Cell(CellIndex(0)), OrderPurpose::GridBuy).quantity,
            Quantity::parse("0.015").unwrap()
        );
        // 主仓只买不卖：批次上的订单全是买单。
        assert!(
            plan.place
                .iter()
                .filter(|intent| matches!(intent.unit, Unit::Batch(_)))
                .all(|intent| intent.side == Side::Buy && !intent.reduce_only)
        );
        assert!(!has_intent(
            &plan,
            Unit::Cell(CellIndex(2)),
            OrderPurpose::GridBuy
        ));
        assert!(plan.waiting.contains(&WaitReason::WaitingForBatch));
    }

    #[test]
    fn b04_enabled_cells_sell_only_what_they_hold() {
        let strategy = core_long_strategy();
        let mut state = two_cells_enabled(&strategy);
        // 第 0 格买回 .005，持有 .01。
        state.cell_mut(CellIndex(0)).unwrap().to_buy_back = Quantity::parse("0.01").unwrap();

        let plan = strategy.plan(&state, &[], &[], Price::parse("95000").unwrap());

        assert_eq!(
            intent(&plan, Unit::Cell(CellIndex(0)), OrderPurpose::GridBuy).quantity,
            Quantity::parse("0.01").unwrap()
        );
        let sell = intent(&plan, Unit::Cell(CellIndex(0)), OrderPurpose::GridSell);
        assert_eq!(sell.limit_price, Price::parse("96000").unwrap());
        assert_eq!(sell.quantity, Quantity::parse("0.005").unwrap());
        assert!(sell.reduce_only);
    }

    #[test]
    fn b05c_a_batch_waits_when_only_the_adjusted_limit_is_below_the_minimum() {
        let strategy = single_batch_core_strategy();
        let mut state = strategy.initial_state();
        // 主仓成交 6.9，还差 .1；两格已启用并全部持有。
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("6.9").unwrap();
        strategy.advance(&mut state, &[]);
        for index in 0..2 {
            state.cell_mut(CellIndex(index)).unwrap().to_buy_back = Quantity::ZERO;
        }
        assert!(state.cells()[..2].iter().all(|cell| cell.enabled));
        assert!(!state.cells()[2].enabled);

        // 自身卖单在 100，主仓买单只能挂 99.99，.1 × 99.99 = 9.999 美元。
        let working = vec![
            working_order(
                "0x01",
                Unit::Cell(CellIndex(0)),
                OrderPurpose::GridSell,
                "101",
                "1",
            ),
            working_order(
                "0x02",
                Unit::Cell(CellIndex(1)),
                OrderPurpose::GridSell,
                "100",
                "1",
            ),
        ];
        let plan = strategy.plan(&state, &working, &[], Price::parse("100").unwrap());

        // 按批次价 100 计算是 10 美元，够最低金额：本批不结束，只是暂时挂不出去。
        assert!(!state.batch(BatchIndex(0)).unwrap().finished);
        assert!(!has_intent(
            &plan,
            Unit::Batch(BatchIndex(0)),
            OrderPurpose::CoreBuy
        ));
        assert!(plan.waiting.contains(&WaitReason::LimitAdjustedByOwnOrder));
        assert!(plan.cancel.is_empty());

        // 卖 100 成交后不再有交叉，主仓在批次价上补齐。
        let mut after_sell = state.clone();
        let mut sell = OrderRecord {
            client_order_id: ClientOrderId::try_new("0x02").unwrap(),
            unit: Unit::Cell(CellIndex(1)),
            purpose: OrderPurpose::GridSell,
            limit_price: Price::parse("100").unwrap(),
            quantity: Quantity::parse("1").unwrap(),
            cumulative_filled: Quantity::ZERO,
            reduce_only: true,
            status: OrderStatus::Open,
        };
        strategy.apply_fill(&mut after_sell, &mut sell, Quantity::parse("1").unwrap());
        let working = vec![working_order(
            "0x01",
            Unit::Cell(CellIndex(0)),
            OrderPurpose::GridSell,
            "101",
            "1",
        )];
        let plan = strategy.plan(&after_sell, &working, &[], Price::parse("100").unwrap());

        let core = intent(&plan, Unit::Batch(BatchIndex(0)), OrderPurpose::CoreBuy);
        assert_eq!(core.limit_price, Price::parse("100").unwrap());
        assert_eq!(core.quantity, Quantity::parse("0.1").unwrap());
        assert!(!plan.waiting.contains(&WaitReason::LimitAdjustedByOwnOrder));

        // 主仓买完后本批结束，98 格随后启用。
        after_sell.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("7").unwrap();
        strategy.advance(&mut after_sell, &[]);
        assert!(after_sell.batch(BatchIndex(0)).unwrap().finished);
        assert!(after_sell.cells().iter().all(|cell| cell.enabled));
    }

    #[test]
    fn b08_a_side_below_the_minimum_is_left_out_until_the_cell_is_whole_again() {
        let strategy = fine_core_long_strategy();
        let mut state = strategy.initial_state();
        // 第 0 格卖出 .0149，只剩 .0001 持有：.0001 × 96000 = 9.6 美元，不足最低金额。
        state.cell_mut(CellIndex(0)).unwrap().enabled = true;
        state.cell_mut(CellIndex(0)).unwrap().to_buy_back = Quantity::parse("0.0149").unwrap();

        let plan = strategy.plan(&state, &[], &[], Price::parse("95000").unwrap());

        assert!(!has_intent(
            &plan,
            Unit::Cell(CellIndex(0)),
            OrderPurpose::GridSell
        ));
        let buy = intent(&plan, Unit::Cell(CellIndex(0)), OrderPurpose::GridBuy);
        assert_eq!(buy.quantity, Quantity::parse("0.0149").unwrap());

        // 买回全部成交后格子恢复整份，按 .015 挂卖。
        let mut buy = order_from(buy, "0x01");
        strategy.apply_fill(&mut state, &mut buy, Quantity::parse("0.0149").unwrap());
        let plan = strategy.plan(&state, &[], &[], Price::parse("95000").unwrap());

        assert!(!has_intent(
            &plan,
            Unit::Cell(CellIndex(0)),
            OrderPurpose::GridBuy
        ));
        let sell = intent(&plan, Unit::Cell(CellIndex(0)), OrderPurpose::GridSell);
        assert_eq!(sell.quantity, Quantity::parse("0.015").unwrap());
        assert_eq!(sell.limit_price, Price::parse("96000").unwrap());
    }

    #[test]
    fn b13_a_new_limit_never_crosses_this_strategys_own_opposite_order() {
        let strategy = five_cell_strategy();
        let mut state = strategy.initial_state();
        // 价格跌到 91500：95、94、93、91 四格已买入并持有，92 格没成交。
        for index in [0, 1, 2, 4] {
            let cell = state.cell_mut(CellIndex(index)).unwrap();
            cell.enabled = true;
            cell.to_buy_back = Quantity::ZERO;
        }
        let working = vec![
            working_order(
                "0x01",
                Unit::Cell(CellIndex(0)),
                OrderPurpose::GridSell,
                "96000",
                "0.05",
            ),
            working_order(
                "0x02",
                Unit::Cell(CellIndex(1)),
                OrderPurpose::GridSell,
                "95000",
                "0.05",
            ),
            working_order(
                "0x03",
                Unit::Cell(CellIndex(2)),
                OrderPurpose::GridSell,
                "94000",
                "0.05",
            ),
            working_order(
                "0x04",
                Unit::Cell(CellIndex(4)),
                OrderPurpose::GridSell,
                "92000",
                "0.05",
            ),
            working_order(
                "0x05",
                Unit::Cell(CellIndex(3)),
                OrderPurpose::GridBuy,
                "92000",
                "0.05",
            ),
        ];

        let plan = strategy.plan(&state, &working, &[], Price::parse("91500").unwrap());

        // 92 格的买单下调到自身最低卖价减一个报价单位，91 格的卖单上移一个报价单位。
        let buy = intent(&plan, Unit::Cell(CellIndex(3)), OrderPurpose::GridBuy);
        assert_eq!(buy.limit_price, Price::parse("91999").unwrap());
        let sell = intent(&plan, Unit::Cell(CellIndex(4)), OrderPurpose::GridSell);
        assert_eq!(sell.limit_price, Price::parse("92001").unwrap());
        // 不相交，两张单不会互相成交。
        assert!(buy.limit_price < sell.limit_price);
        // 在场订单价格变了，先撤掉再挂。
        assert_eq!(plan.cancel.len(), 2);
        // 高处的卖单不受影响。
        assert!(!has_intent(
            &plan,
            Unit::Cell(CellIndex(0)),
            OrderPurpose::GridSell
        ));
    }

    #[test]
    fn b13_limits_stay_at_the_preset_prices_when_nothing_crosses() {
        let strategy = five_cell_strategy();
        let mut state = strategy.initial_state();
        // 高处的三格持有等卖出，92 格卖完了等买回：买价 92000 低于最低卖价 94000，
        // 不相交，各自的限价都不动。
        for (index, to_buy_back) in [(0, "0"), (1, "0"), (2, "0"), (3, "0.05")] {
            let cell = state.cell_mut(CellIndex(index)).unwrap();
            cell.enabled = true;
            cell.to_buy_back = Quantity::parse(to_buy_back).unwrap();
        }

        let plan = strategy.plan(&state, &[], &[], Price::parse("91500").unwrap());

        assert_eq!(
            intent(&plan, Unit::Cell(CellIndex(3)), OrderPurpose::GridBuy).limit_price,
            Price::parse("92000").unwrap()
        );
        for (index, price) in [(0, "96000"), (1, "95000"), (2, "94000")] {
            assert_eq!(
                intent(&plan, Unit::Cell(CellIndex(index)), OrderPurpose::GridSell).limit_price,
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
            state.cell_mut(CellIndex(index)).unwrap().to_buy_back = Quantity::ZERO;
        }

        let plan = strategy.plan(&state, &[], &[], Price::parse("91500").unwrap());

        let buy = intent(&plan, Unit::Cell(CellIndex(3)), OrderPurpose::GridBuy);
        assert_eq!(buy.limit_price, Price::parse("92000").unwrap());
        let sell = intent(&plan, Unit::Cell(CellIndex(4)), OrderPurpose::GridSell);
        assert_eq!(sell.limit_price, Price::parse("92001").unwrap());
        assert!(buy.limit_price < sell.limit_price);

        // 下一步两张单都还在场，价格不再变化。
        let working = vec![
            working_order(
                "0x01",
                Unit::Cell(CellIndex(0)),
                OrderPurpose::GridSell,
                "96000",
                "0.05",
            ),
            working_order(
                "0x02",
                Unit::Cell(CellIndex(1)),
                OrderPurpose::GridSell,
                "95000",
                "0.05",
            ),
            working_order(
                "0x03",
                Unit::Cell(CellIndex(2)),
                OrderPurpose::GridSell,
                "94000",
                "0.05",
            ),
            working_order(
                "0x04",
                Unit::Cell(CellIndex(3)),
                OrderPurpose::GridBuy,
                "92000",
                "0.05",
            ),
            working_order(
                "0x05",
                Unit::Cell(CellIndex(4)),
                OrderPurpose::GridSell,
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
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.02").unwrap();
        strategy.advance(&mut state, &[]);
        assert!(state.batch(BatchIndex(0)).unwrap().finished);
        assert!(!state.cells()[0].enabled);

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
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.175").unwrap();
        strategy.advance(&mut state, &[]);
        assert!(state.batch(BatchIndex(0)).unwrap().finished);
        // 上一次规划按 `C − F = .105` 挂出去的那张单还在场。
        let working = vec![working_order(
            "0x01",
            Unit::Batch(BatchIndex(0)),
            OrderPurpose::CoreBuy,
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
            Unit::Batch(BatchIndex(0)),
            OrderPurpose::CoreBuy
        ));
        // 其他批次不受影响。
        assert!(has_intent(
            &plan,
            Unit::Batch(BatchIndex(1)),
            OrderPurpose::CoreBuy
        ));
    }

    #[test]
    fn working_orders_that_already_match_are_kept() {
        let strategy = core_long_strategy();
        let state = two_cells_enabled(&strategy);
        let working = vec![
            working_order(
                "0x01",
                Unit::Batch(BatchIndex(0)),
                OrderPurpose::CoreBuy,
                "95000",
                "0.105",
            ),
            working_order(
                "0x02",
                Unit::Cell(CellIndex(0)),
                OrderPurpose::GridBuy,
                "95000",
                "0.015",
            ),
        ];

        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());

        assert!(plan.cancel.is_empty());
        assert!(!has_intent(
            &plan,
            Unit::Batch(BatchIndex(0)),
            OrderPurpose::CoreBuy
        ));
        assert!(!has_intent(
            &plan,
            Unit::Cell(CellIndex(0)),
            OrderPurpose::GridBuy
        ));
        // 其余批次和格子照常挂出。
        assert!(has_intent(
            &plan,
            Unit::Batch(BatchIndex(1)),
            OrderPurpose::CoreBuy
        ));
        assert!(has_intent(
            &plan,
            Unit::Cell(CellIndex(1)),
            OrderPurpose::GridBuy
        ));
    }

    #[test]
    fn working_orders_with_a_stale_price_or_size_are_canceled() {
        let strategy = core_long_strategy();
        let state = two_cells_enabled(&strategy);
        let working = vec![
            working_order(
                "0x01",
                Unit::Batch(BatchIndex(0)),
                OrderPurpose::CoreBuy,
                "95000",
                "0.2",
            ),
            working_order(
                "0x02",
                Unit::Cell(CellIndex(0)),
                OrderPurpose::GridBuy,
                "94000",
                "0.015",
            ),
        ];

        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());

        let mut canceled: Vec<&str> = plan.cancel.iter().map(|id| id.as_str()).collect();
        canceled.sort_unstable();
        assert_eq!(canceled, vec!["0x01", "0x02"]);
        assert_eq!(
            intent(&plan, Unit::Batch(BatchIndex(0)), OrderPurpose::CoreBuy).quantity,
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
            Unit::Batch(BatchIndex(0)),
            OrderPurpose::CoreBuy,
            "95000",
            "0.1",
        )];

        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());

        assert_eq!(
            intent(&plan, Unit::Batch(BatchIndex(0)), OrderPurpose::CoreBuy).quantity,
            Quantity::parse("0.005").unwrap()
        );

        // 在场订单已经覆盖本批还差的部分：不重复挂单。
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.075").unwrap();
        let plan = strategy.plan(&state, &working, &[], Price::parse("96000").unwrap());
        assert!(!has_intent(
            &plan,
            Unit::Batch(BatchIndex(0)),
            OrderPurpose::CoreBuy
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
            Unit::Batch(BatchIndex(0)),
            OrderPurpose::CoreBuy,
            "95000",
            "0.2",
        )];
        let pending = [PendingUnit {
            unit: Unit::Batch(BatchIndex(0)),
            purpose: OrderPurpose::CoreBuy,
        }];

        let plan = strategy.plan(&state, &working, &pending, Price::parse("96000").unwrap());

        // 在场订单的数量对不上，照常撤掉。
        assert_eq!(plan.cancel, vec![ClientOrderId::try_new("0x01").unwrap()]);
        // 但这一批的主仓买单不补：等对完账再按新的状态算数量。
        assert!(!has_intent(
            &plan,
            Unit::Batch(BatchIndex(0)),
            OrderPurpose::CoreBuy
        ));
        assert!(plan.waiting.contains(&WaitReason::FillsPending));
        // 其他批次和其他用途不受影响。
        assert!(has_intent(
            &plan,
            Unit::Batch(BatchIndex(1)),
            OrderPurpose::CoreBuy
        ));
        assert!(has_intent(
            &plan,
            Unit::Cell(CellIndex(0)),
            OrderPurpose::GridBuy
        ));
    }
}
