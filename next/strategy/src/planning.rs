//! 规划：给定状态、在场订单和当前价格，输出要下的单、要撤的单和等待原因。
//!
//! 规则（做多侧、没有主仓、不限速）：
//! - 每个格子启用后自成单位，状态为全部待买回，在格子买价挂买单；
//! - 持有部分在卖价挂卖单（只减仓），待买回部分在买价挂买单；
//! - 某一侧剩余量不足最低订单金额时，这一侧先不挂单，只挂另一侧。

use serde::{Deserialize, Serialize};

use crate::Strategy;
use crate::grid::CellIndex;
use crate::order::{ClientOrderId, OrderPurpose, Side, WorkingOrder};
use crate::state::StrategyState;
use crate::units::{Price, Quantity};

/// 要下的一张订单：身份和客户端订单 ID 由 runtime 生成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderIntent {
    pub cell_index: CellIndex,
    pub purpose: OrderPurpose,
    pub side: Side,
    pub limit_price: Price,
    pub quantity: Quantity,
    pub reduce_only: bool,
}

/// 策略当前不发出新订单的原因。
///
/// 这是设计第 11 节列出的完整集合。本任务支持的配置（做多、没有主仓、不限速）
/// 不会产生任何一项：格子启用后立即挂单，等待只发生在格子内部（某一侧不足最低金额）。
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

/// 格子某一侧应当在场的一张订单。
struct SideDemand {
    cell_index: CellIndex,
    purpose: OrderPurpose,
    limit_price: Price,
    quantity: Quantity,
    reduce_only: bool,
}

impl Strategy {
    /// 规划下一步动作。纯计算，不读写任何外部状态。
    ///
    /// `market_price` 是当前标记价。本任务的规则不读它：挂单都挂在预设价格上，
    /// 是否立即成交由交易所决定。它记在 [`Plan`] 里，供预览和日志使用。
    pub fn plan(
        &self,
        state: &StrategyState,
        working_orders: &[WorkingOrder],
        market_price: Price,
    ) -> Plan {
        let mut plan = Plan {
            market_price,
            ..Plan::default()
        };
        for cell in self.grid().cells() {
            let Some(cell_state) = state.cell(cell.index) else {
                continue;
            };
            if !cell_state.enabled {
                continue;
            }
            let held = cell.quantity.saturating_sub(cell_state.to_buy_back);
            let demands = [
                SideDemand {
                    cell_index: cell.index,
                    purpose: OrderPurpose::GridBuy,
                    limit_price: cell.buy_price,
                    quantity: cell_state.to_buy_back,
                    reduce_only: false,
                },
                SideDemand {
                    cell_index: cell.index,
                    purpose: OrderPurpose::GridSell,
                    limit_price: cell.sell_price,
                    quantity: held,
                    reduce_only: true,
                },
            ];
            for demand in &demands {
                self.reconcile_side(working_orders, demand, &mut plan);
            }
        }
        plan
    }

    /// 让某一侧的在场订单与应当在场的数量一致：价格或数量不符的撤掉，缺的补上。
    fn reconcile_side(
        &self,
        working_orders: &[WorkingOrder],
        demand: &SideDemand,
        plan: &mut Plan,
    ) {
        let mut kept = Quantity::ZERO;
        for order in working_orders.iter().filter(|order| {
            order.cell_index == demand.cell_index && order.purpose == demand.purpose
        }) {
            if order.limit_price == demand.limit_price && kept + order.remaining <= demand.quantity
            {
                kept = kept + order.remaining;
            } else {
                plan.cancel.push(order.client_order_id.clone());
            }
        }

        let missing = demand.quantity.saturating_sub(kept);
        if missing.is_zero() {
            return;
        }
        // 剩余量不足最低订单金额时这一侧先不挂单（第 4.4 节）。
        if !self.spec().meets_minimum_order(missing, demand.limit_price) {
            return;
        }
        plan.place.push(OrderIntent {
            cell_index: demand.cell_index,
            purpose: demand.purpose,
            side: demand.purpose.side(),
            limit_price: demand.limit_price,
            quantity: missing,
            reduce_only: demand.reduce_only,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{OrderIntent, Plan};
    use crate::Strategy;
    use crate::fixtures::{btc_fees, btc_spec, long_grid_config, long_grid_strategy};
    use crate::grid::CellIndex;
    use crate::instrument::{Instrument, InstrumentSpec};
    use crate::order::{ClientOrderId, OrderPurpose, Side, WorkingOrder};
    use crate::state::StrategyState;
    use crate::units::{Notional, Price, Quantity};

    fn intent(plan: &Plan, cell_index: usize) -> Vec<&OrderIntent> {
        plan.place
            .iter()
            .filter(|intent| intent.cell_index == CellIndex(cell_index))
            .collect()
    }

    fn working_order(
        id: &str,
        cell_index: usize,
        purpose: OrderPurpose,
        limit_price: &str,
        remaining: &str,
    ) -> WorkingOrder {
        WorkingOrder {
            client_order_id: ClientOrderId::try_new(id).unwrap(),
            cell_index: CellIndex(cell_index),
            purpose,
            limit_price: Price::parse(limit_price).unwrap(),
            remaining: Quantity::parse(remaining).unwrap(),
        }
    }

    fn state_with_to_buy_back(cell_index: usize, to_buy_back: &str) -> StrategyState {
        let strategy = long_grid_strategy();
        let mut state = strategy.initial_state();
        state.cell_mut(CellIndex(cell_index)).unwrap().to_buy_back =
            Quantity::parse(to_buy_back).unwrap();
        state
    }

    #[test]
    fn b24_every_cell_starts_as_its_own_unit_with_a_buy_at_its_price() {
        let strategy = long_grid_strategy();

        let plan = strategy.plan(
            &strategy.initial_state(),
            &[],
            Price::parse("96000").unwrap(),
        );

        assert_eq!(plan.place.len(), 20);
        assert!(plan.cancel.is_empty());
        assert!(plan.waiting.is_empty());
        let highest = intent(&plan, 0);
        assert_eq!(highest.len(), 1);
        assert_eq!(highest[0].purpose, OrderPurpose::GridBuy);
        assert_eq!(highest[0].side, Side::Buy);
        assert_eq!(highest[0].limit_price, Price::parse("95000").unwrap());
        assert_eq!(highest[0].quantity, Quantity::parse("0.05").unwrap());
        assert!(!highest[0].reduce_only);
        let lowest = intent(&plan, 19);
        assert_eq!(lowest[0].limit_price, Price::parse("76000").unwrap());
    }

    #[test]
    fn b07_partially_sold_cell_keeps_a_sell_and_a_buy_back_at_different_prices() {
        let strategy = long_grid_strategy();
        // 95 格买入 0.02 后部分成交：待买回 0.03，持有 0.02。
        let state = state_with_to_buy_back(0, "0.03");
        let working = vec![working_order(
            "0x01",
            0,
            OrderPurpose::GridBuy,
            "95000",
            "0.03",
        )];

        let plan = strategy.plan(&state, &working, Price::parse("94000").unwrap());

        let orders = intent(&plan, 0);
        assert_eq!(
            orders.len(),
            1,
            "the working buy back already covers the rest"
        );
        assert_eq!(orders[0].purpose, OrderPurpose::GridSell);
        assert_eq!(orders[0].limit_price, Price::parse("96000").unwrap());
        assert_eq!(orders[0].quantity, Quantity::parse("0.02").unwrap());
        assert!(orders[0].reduce_only);
        assert!(plan.cancel.is_empty());
    }

    #[test]
    fn planning_places_only_the_missing_remainder_next_to_a_working_order() {
        let strategy = long_grid_strategy();
        let state = strategy.initial_state();
        let working = vec![working_order(
            "0x01",
            0,
            OrderPurpose::GridBuy,
            "95000",
            "0.02",
        )];

        let plan = strategy.plan(&state, &working, Price::parse("96000").unwrap());

        let orders = intent(&plan, 0);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].quantity, Quantity::parse("0.03").unwrap());
    }

    #[test]
    fn planning_cancels_working_orders_that_no_longer_match_the_plan() {
        let strategy = long_grid_strategy();
        let state = strategy.initial_state();
        let working = vec![working_order(
            "0x01",
            0,
            OrderPurpose::GridBuy,
            "94000",
            "0.05",
        )];

        let plan = strategy.plan(&state, &working, Price::parse("96000").unwrap());

        assert_eq!(plan.cancel, vec![ClientOrderId::try_new("0x01").unwrap()]);
        let orders = intent(&plan, 0);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].limit_price, Price::parse("95000").unwrap());
        assert_eq!(orders[0].quantity, Quantity::parse("0.05").unwrap());
    }

    #[test]
    fn cell_side_below_the_minimum_order_amount_is_left_unordered() {
        let spec = InstrumentSpec::new(
            Instrument::try_new("BTC").unwrap(),
            Price::parse("1").unwrap(),
            Quantity::parse("0.001").unwrap(),
            Notional::parse("1000").unwrap(),
        )
        .unwrap();
        let strategy = Strategy::new(long_grid_config(), spec, btc_fees()).unwrap();
        // 95 格只剩 0.001 待买回（95 美元），低于最低订单金额；持有 0.049 照常挂卖。
        let state = state_with_to_buy_back(0, "0.001");

        let plan = strategy.plan(&state, &[], Price::parse("94000").unwrap());

        let orders = intent(&plan, 0);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].purpose, OrderPurpose::GridSell);
        assert_eq!(orders[0].quantity, Quantity::parse("0.049").unwrap());
        assert!(plan.cancel.is_empty());
    }

    #[test]
    fn planning_reports_no_wait_reason_for_a_long_grid_without_core_or_throttle() {
        let strategy = long_grid_strategy();

        let plan = strategy.plan(
            &strategy.initial_state(),
            &[],
            Price::parse("96000").unwrap(),
        );

        assert_eq!(plan.market_price, Price::parse("96000").unwrap());
        assert!(plan.waiting.is_empty());
        assert_eq!(strategy.spec(), &btc_spec());
        assert!(strategy.grid().len() == 20);
    }
}
