//! 成交记账：按订单所属格子和用途更新状态。
//!
//! 去重规则（第 9.3 节）：同一订单的累计成交量只接受更大的值。因此
//! 重复、乱序和累计回退的成交都只会记一次，且结果与到达顺序无关。

use chrono::{DateTime, Utc};

use crate::Strategy;
use crate::order::{OrderPurpose, OrderRecord, OrderStatus, Unit};
use crate::state::StrategyState;
use crate::units::Quantity;

/// 一次成交记账的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillApplication {
    /// 本次新记入的数量。
    pub applied: Quantity,
    /// 记账后该订单的累计成交量。
    pub cumulative_filled: Quantity,
}

impl FillApplication {
    pub fn is_duplicate(&self) -> bool {
        self.applied.is_zero()
    }
}

impl Strategy {
    /// 把一张订单的累计成交量记入它所属的格子。
    ///
    /// `cumulative` 是交易所给出的该订单累计成交量；小于已记录的累计量时忽略。
    /// 记账后订单全部成交时，订单状态改为已成交。
    ///
    /// `filled_at` 是这笔成交在交易所发生的时刻，用来算限速的放行时刻（第 4.1 节）；
    /// 只有真的记入了新数量才算数，重复和迟到的成交不会重新计时。
    pub fn apply_fill(
        &self,
        state: &mut StrategyState,
        order: &mut OrderRecord,
        cumulative: Quantity,
        filled_at: DateTime<Utc>,
    ) -> FillApplication {
        let reported = cumulative.min(order.quantity).max(Quantity::ZERO);
        let effective = order.cumulative_filled.max(reported);
        let applied = effective.saturating_sub(order.cumulative_filled);
        order.cumulative_filled = effective;

        if !applied.is_zero() {
            match order.unit {
                Unit::Batch(batch_index) => {
                    // 主仓只买入，永不为它下平仓单（第 7.1 节）。
                    let core_quantity = self
                        .batches()
                        .batch(batch_index)
                        .map(|batch| batch.core_quantity)
                        .unwrap_or(Quantity::ZERO);
                    if let Some(batch_state) = state.batch_mut(batch_index) {
                        batch_state.filled = batch_state
                            .filled
                            .saturating_add(applied)
                            .min(core_quantity);
                    }
                }
                Unit::Cell(cell_index) => {
                    let cell_quantity = self
                        .grid()
                        .cell(cell_index)
                        .map(|cell| cell.quantity)
                        .unwrap_or(Quantity::ZERO);
                    if let Some(cell_state) = state.cell_mut(cell_index) {
                        match order.purpose {
                            OrderPurpose::GridBuy => {
                                cell_state.to_buy_back =
                                    cell_state.to_buy_back.saturating_sub(applied);
                            }
                            OrderPurpose::GridSell => {
                                cell_state.to_buy_back = cell_state
                                    .to_buy_back
                                    .saturating_add(applied)
                                    .min(cell_quantity);
                            }
                            OrderPurpose::CoreBuy => {}
                        }
                    }
                }
            }
            // 限速的放行时刻从「最近放行单位的第一笔成交」算起。
            self.record_fill_time(state, order.unit, filled_at);
        }

        // 全部成交就是全部成交：即使这张单先被判成撤销，补记的成交也要把它改回已成交。
        if order.remaining().is_zero() {
            order.status = OrderStatus::Filled;
        }

        FillApplication {
            applied,
            cumulative_filled: order.cumulative_filled,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Strategy;
    use crate::batch::BatchIndex;
    use crate::fixtures::{at, core_long_strategy, long_grid_strategy};
    use crate::grid::CellIndex;
    use crate::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Unit};
    use crate::state::StrategyState;
    use crate::units::{Price, Quantity};

    fn order(purpose: OrderPurpose, quantity: &str) -> OrderRecord {
        OrderRecord {
            client_order_id: ClientOrderId::try_new("0x01").unwrap(),
            unit: Unit::Cell(CellIndex(0)),
            purpose,
            limit_price: Price::parse("95000").unwrap(),
            quantity: Quantity::parse(quantity).unwrap(),
            cumulative_filled: Quantity::ZERO,
            reduce_only: purpose == OrderPurpose::GridSell,
            status: OrderStatus::Open,
        }
    }

    fn fill(
        strategy: &Strategy,
        state: &mut StrategyState,
        order: &mut OrderRecord,
        cumulative: &str,
    ) -> Quantity {
        strategy
            .apply_fill(state, order, Quantity::parse(cumulative).unwrap(), at(0))
            .applied
    }

    /// 第 0 批的主仓买单，数量是 `C = .175`。
    fn core_order(quantity: &str) -> OrderRecord {
        OrderRecord {
            client_order_id: ClientOrderId::try_new("0x02").unwrap(),
            unit: Unit::Batch(BatchIndex(0)),
            purpose: OrderPurpose::CoreBuy,
            limit_price: Price::parse("95000").unwrap(),
            quantity: Quantity::parse(quantity).unwrap(),
            cumulative_filled: Quantity::ZERO,
            reduce_only: false,
            status: OrderStatus::Open,
        }
    }

    #[test]
    fn b17_duplicate_out_of_order_and_regressed_fills_are_applied_once() {
        let strategy = long_grid_strategy();
        let mut state = strategy.initial_state();
        let mut order = order(OrderPurpose::GridBuy, "0.05");

        assert_eq!(
            fill(&strategy, &mut state, &mut order, "0.02"),
            Quantity::parse("0.02").unwrap()
        );
        // 重复的同一条成交
        assert_eq!(
            fill(&strategy, &mut state, &mut order, "0.02"),
            Quantity::ZERO
        );
        // 累计量回退
        assert_eq!(
            fill(&strategy, &mut state, &mut order, "0.01"),
            Quantity::ZERO
        );
        // 乱序：先到 0.04，迟到的 0.03 只补差额
        assert_eq!(
            fill(&strategy, &mut state, &mut order, "0.04"),
            Quantity::parse("0.02").unwrap()
        );
        assert_eq!(
            fill(&strategy, &mut state, &mut order, "0.03"),
            Quantity::ZERO
        );
        assert_eq!(order.cumulative_filled, Quantity::parse("0.04").unwrap());
        assert_eq!(
            fill(&strategy, &mut state, &mut order, "0.05"),
            Quantity::parse("0.01").unwrap()
        );
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(
            state.cell(CellIndex(0)).unwrap().to_buy_back,
            Quantity::ZERO
        );
    }

    #[test]
    fn b17_cell_state_is_independent_of_fill_arrival_order() {
        let strategy = long_grid_strategy();
        let orders = ["0.01", "0.03", "0.02", "0.04", "0.05"];
        let mut final_states = Vec::new();
        for ordering in [[0, 1, 2, 3, 4], [4, 3, 2, 1, 0], [2, 0, 4, 1, 3]] {
            let mut order = order(OrderPurpose::GridBuy, "0.05");
            let mut state = strategy.initial_state();
            for index in ordering {
                strategy.apply_fill(
                    &mut state,
                    &mut order,
                    Quantity::parse(orders[index]).unwrap(),
                    at(0),
                );
            }
            final_states.push((state, order.clone()));
        }

        assert_eq!(final_states[0], final_states[1]);
        assert_eq!(final_states[1], final_states[2]);
        assert_eq!(
            final_states[0].0.cell(CellIndex(0)).unwrap().to_buy_back,
            Quantity::ZERO
        );
        assert_eq!(
            strategy.expected_position(&final_states[0].0),
            crate::units::Position::parse("0.05").unwrap()
        );
    }

    #[test]
    fn grid_sell_fills_move_quantity_back_to_to_buy_back() {
        let strategy = long_grid_strategy();
        let mut state = strategy.initial_state();
        state.cell_mut(CellIndex(0)).unwrap().to_buy_back = Quantity::parse("0.02").unwrap();
        let mut order = order(OrderPurpose::GridSell, "0.03");
        assert_eq!(
            strategy.expected_position(&state),
            crate::units::Position::parse("0.03").unwrap()
        );

        let application = strategy.apply_fill(
            &mut state,
            &mut order,
            Quantity::parse("0.03").unwrap(),
            at(0),
        );

        assert_eq!(application.applied, Quantity::parse("0.03").unwrap());
        assert_eq!(
            state.cell(CellIndex(0)).unwrap().to_buy_back,
            Quantity::parse("0.05").unwrap()
        );
        assert_eq!(
            strategy.expected_position(&state),
            crate::units::Position::ZERO
        );
    }

    #[test]
    fn fills_above_the_order_quantity_are_clamped() {
        let strategy = long_grid_strategy();
        let mut order = order(OrderPurpose::GridBuy, "0.05");

        let application = strategy.apply_fill(
            &mut strategy.initial_state(),
            &mut order,
            Quantity::parse("0.06").unwrap(),
            at(0),
        );

        assert_eq!(application.applied, Quantity::parse("0.05").unwrap());
        assert_eq!(order.cumulative_filled, Quantity::parse("0.05").unwrap());
    }

    #[test]
    fn core_buy_fills_add_to_their_batch_and_stop_at_the_core_quantity() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();
        let mut order = core_order("0.175");

        fill(&strategy, &mut state, &mut order, "0.07");
        assert_eq!(
            state.batch(BatchIndex(0)).unwrap().filled,
            Quantity::parse("0.07").unwrap()
        );
        assert_eq!(state.batch(BatchIndex(1)).unwrap().filled, Quantity::ZERO);
        assert_eq!(
            strategy.expected_position(&state),
            crate::units::Position::parse("0.07").unwrap()
        );

        // 超过 C 的部分不计入：主仓不会超过配置的比例。
        fill(&strategy, &mut state, &mut order, "0.2");
        assert_eq!(
            state.batch(BatchIndex(0)).unwrap().filled,
            Quantity::parse("0.175").unwrap()
        );
    }

    #[test]
    fn grid_orders_never_touch_the_core() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();
        state.batch_mut(BatchIndex(0)).unwrap().filled = Quantity::parse("0.175").unwrap();
        strategy.advance(&mut state, &[]);
        let mut order = order(OrderPurpose::GridSell, "0.015");

        fill(&strategy, &mut state, &mut order, "0.015");

        assert_eq!(
            state.batch(BatchIndex(0)).unwrap().filled,
            Quantity::parse("0.175").unwrap()
        );
        assert_eq!(
            strategy.expected_position(&state),
            crate::units::Position::parse("0.175").unwrap()
        );
    }
}
