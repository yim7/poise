//! 结果未知的订单：期限过后按挂单、成交、仓位三项核对，判定没有影响就作废（第 9.2 节）。

use chrono::{DateTime, Utc};
use poise_strategy::decision::{MissingFact, PositionCheck, missing_facts};
use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderStatus};
use poise_strategy::units::Quantity;

use crate::store::{StoredOrder, StrategyId};

use super::{Runtime, RuntimeError};

/// 一轮核对之后结果未知的订单。
pub(super) struct UnknownOrders {
    /// 这一轮判定为没有任何影响、已经作废的订单。
    pub(super) voided: Vec<ClientOrderId>,
    /// 还没有结论的订单：它的期限，以及还缺的事实。
    pub(super) pending: Vec<PendingUnknown>,
}

pub(super) struct PendingUnknown {
    client_order_id: ClientOrderId,
    /// `expiresAfter` 加安全余量：过了这一刻请求不会再落地。
    deadline: DateTime<Utc>,
    missing: Vec<MissingFact>,
}

impl UnknownOrders {
    /// 本策略还有没有结果未知的订单：有就不发任何新订单（第 7.2 节）。
    pub(super) fn any_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 写给用户看的说明：哪几张单结果未知、各自缺什么（第 9.2 节第 4 条）。
    pub(super) fn note(&self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let orders: Vec<String> = self
            .pending
            .iter()
            .map(|pending| {
                let missing: Vec<String> = pending
                    .missing
                    .iter()
                    .map(|fact| match fact {
                        MissingFact::NeverConfirmed => "曾确认在场，等交易所给出终态".to_string(),
                        MissingFact::Deadline => {
                            format!("等期限 {}", pending.deadline)
                        }
                        MissingFact::Fills => "有成交记录".to_string(),
                        MissingFact::Position => "仓位未核对一致".to_string(),
                    })
                    .collect();
                format!("{}：{}", pending.client_order_id, missing.join("、"))
            })
            .collect();
        Some(format!(
            "{} 张订单结果未知（交易所查无此单），挂单、成交、仓位核对一致前不发新单。{}",
            self.pending.len(),
            orders.join("；")
        ))
    }
}

impl Runtime {
    /// 按三项核对处理结果未知的订单：期限已过、没有成交、仓位一致，就判定它没有任何影响，
    /// 作废并释放预留；缺任何一项都保持原样（第 9.2 节第 3、4 条）。
    ///
    /// 只有交易所从没确认过的订单（没有交易所订单编号）才可能作废：确认过在场、后来又查
    /// 不到的，可能只是查询滞后，它仍然可能成交（见 [`missing_facts`]）。
    ///
    /// 「当前挂单中没有它」由对账保证：只有挂单列表和按 `cloid` 查询都找不到的订单，状态
    /// 才是 [`OrderStatus::Unknown`]。成交要看从发送到现在的完整流水：补拉成交的回查起点
    /// 不晚于在场订单挂出去的时刻（[`Runtime::fills_since`]），所以调用方必须在这一步的
    /// 成交同步成功之后调它。仓位要用这一步刚核对出的结论，`position` 不是
    /// [`PositionCheck::Consistent`] 时一张都不作废。
    ///
    /// 市价平仓单（第 7.7 节）不看仓位这一项：它只减仓、立即成交否则撤销，过了期限不会再
    /// 成交；下一张平仓单按交易所的实时仓位算，这张单就算悄悄成交过，实时仓位里也已经扣掉
    /// 了，交易所的只减仓限制保证不会平过头。账上的应有持仓在平仓时本来就可能和实际仓位
    /// 不一致（归档之后手动交易过、仓位核对失败暂停过、平仓途中手动交易），要求仓位一致，
    /// 这张单就永远作废不了，平仓和放弃平仓的收尾都会卡住。
    ///
    /// 作废之后不换 `cloid` 重发：它的单位按状态重新规划，下一张单是新的订单。
    pub(super) fn settle_unknown_orders(
        &self,
        strategy_id: &StrategyId,
        orders: &mut [StoredOrder],
        position: PositionCheck,
        now: DateTime<Utc>,
    ) -> Result<UnknownOrders, RuntimeError> {
        let mut unknown = UnknownOrders {
            voided: Vec::new(),
            pending: Vec::new(),
        };
        if !orders
            .iter()
            .any(|order| order.record.status == OrderStatus::Unknown)
        {
            return Ok(unknown);
        }
        let saved_fills = self.store.fills_totals(strategy_id)?;
        for order in orders
            .iter_mut()
            .filter(|order| order.record.status == OrderStatus::Unknown)
        {
            let deadline = order.expires_at + self.options.unknown_order_grace;
            let position = if order.record.purpose == OrderPurpose::Close {
                PositionCheck::Consistent
            } else {
                position
            };
            let missing = missing_facts(
                order.exchange_order_id.is_some(),
                now >= deadline,
                order.record.cumulative_filled,
                saved_fills
                    .get(&order.record.client_order_id)
                    .copied()
                    .unwrap_or(Quantity::ZERO),
                position,
            );
            if missing.is_empty() {
                order.record.status = OrderStatus::Void;
                // 没有成交，也就没有要对的账。
                order.fills_reconciled = true;
                order.updated_at = now;
                self.store.update_order(strategy_id, order)?;
                tracing::warn!(
                    strategy = %strategy_id,
                    client_order_id = %order.record.client_order_id,
                    "an order with an unknown result had no effect, releasing its reservation"
                );
                unknown.voided.push(order.record.client_order_id.clone());
            } else {
                unknown.pending.push(PendingUnknown {
                    client_order_id: order.record.client_order_id.clone(),
                    deadline,
                    missing,
                });
            }
        }
        Ok(unknown)
    }
}
