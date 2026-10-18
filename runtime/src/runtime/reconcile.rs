//! 与交易所对账：补记成交、按 `cloid` 核对订单终态、撤单（第 9.2、9.3 节）。

use chrono::{DateTime, Utc};
use poise_strategy::Strategy;
use poise_strategy::decision::{fills_settled, inconsistent_fill_records, unbooked_quantity};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderRecord, OrderStatus};
use poise_strategy::state::StrategyState;
use poise_strategy::units::Quantity;

use crate::exchange::{ExchangeError, ExchangeOrder, OrderLookup};
use crate::store::{StoredFill, StoredOrder, StoredStrategy, StrategyId};

use super::cancel_only::outcome_unknown;
use super::{ExchangeFacts, ReconcileOutcome, Runtime, RuntimeError, order_states};

/// 拉成交流水最多往前查多久（[`Runtime::fills_since`]）。
///
/// 流水只是审计记录，漏掉更早的不影响记账；而 Hyperliquid 的成交接口只留整个账户最近
/// 一万笔，查得太远就永远查不全。按设计第 10 节的估算，网格每天的操作不到两千次，
/// 一小时远到不了一万笔。
pub(super) const FILL_STREAM_MAX_WINDOW: chrono::Duration = chrono::Duration::hours(1);

impl Runtime {
    /// 与交易所核对订单：收尾不在挂单列表里的订单、吸收它们已经成交的部分。
    ///
    /// 返回交易所报的累计成交量快照，以及这次从订单状态里补记了几笔成交；查不到的在场
    /// 订单记成 [`OrderStatus::Unknown`]。快照供仓位核对使用：成交记录可能比挂单状态晚到，
    /// 这段差额是能解释的（第 7.1.3 节）。
    ///
    /// 一张单完全成交后同样会消失，所以不能只凭「过了 `expiresAfter`」判它撤销，
    /// 要问交易所它是什么结局（第 9.2 节）。
    ///
    /// 核对范围是「还在场」加「还没对完账」的订单：撤销只说明它不在场了，撤销前成交了
    /// 多少要等交易所给出最终累计量。交易所给的累计量直接记账，不等成交流水（第 9.3 节）。
    pub(super) async fn reconcile_orders(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        now: DateTime<Utc>,
    ) -> Result<ReconcileOutcome, RuntimeError> {
        let instrument = strategy.spec().instrument().clone();
        let exchange_working = self.exchange.working_orders(&instrument).await?;
        // 挂单列表的接收时刻：列表里的订单直接用它当成交时刻的上界。
        let working_observed_at = self.clock.now();
        let saved_fills = self.store.fills_totals(strategy_id)?;
        let mut reported: Vec<(ClientOrderId, Quantity)> = Vec::new();
        let mut applied_fills = 0;

        for order in orders
            .iter_mut()
            .filter(|order| order.record.is_working() || !order.fills_reconciled)
        {
            let (exchange_view, observed_at) = match exchange_working
                .iter()
                .find(|working| {
                    working.client_order_id.as_ref() == Some(&order.record.client_order_id)
                })
                .cloned()
            {
                Some(working) => (working, working_observed_at),
                None => {
                    match self
                        .exchange
                        .order_status(&instrument, &order.record.client_order_id)
                        .await?
                    {
                        OrderLookup::Found(found) => (found, self.clock.now()),
                        // 交易所查不到这张单。凭过期就断定它没有影响是不安全的：保持预留、
                        // 不释放、不重发，记成「结果未知」。下单超时、写库后没来得及发送、
                        // 或者原来在场的单查不到了，都是这一类；期限过后由三项核对决定能不能
                        // 作废（[`Runtime::settle_unknown_orders`]，第 9.2 节）。
                        //
                        // 已经终态的单不改：它的结局我们知道，只是成交还没对完账。
                        OrderLookup::Unknown => {
                            if order.record.is_working()
                                && order.record.status != OrderStatus::Unknown
                            {
                                order.record.status = OrderStatus::Unknown;
                                order.updated_at = now;
                                self.store.update_order(strategy_id, order)?;
                            }
                            continue;
                        }
                    }
                }
            };
            reported.push((
                order.record.client_order_id.clone(),
                exchange_view.cumulative_filled,
            ));
            let facts = ExchangeFacts {
                view: &exchange_view,
                observed_at,
            };
            if self
                .settle_order(strategy_id, strategy, state, order, &facts, now)
                .await?
            {
                applied_fills += 1;
            }
        }
        Ok(ReconcileOutcome {
            reported,
            applied_fills,
            inconsistent: inconsistent_fill_records(&saved_fills, &order_states(orders)),
        })
    }

    /// 按交易所给的订单状态收尾一张订单：把交易所认到的累计成交量记入格子，更新本地
    /// 记录。返回是否新记了一笔成交。
    ///
    /// 累计成交量以订单状态为准，在场的单和终态的单一样直接记账，不等成交流水（第 9.3 节）。
    /// 测试网实测：部分成交后撤销的单，按 `cloid` 查到的原始数量减剩余数量就是成交量，
    /// 和流水、仓位一致（设计附录 A）。
    ///
    /// 订单状态里没有成交时间，只能用拿到这份状态的时刻当上界：成交发生在查询之前，
    /// 所以接收时刻一定不早于真实成交时刻，限速按它算只会放行得偏晚。流水到了会把它改回
    /// 真实时刻（[`Strategy::record_fill_time`] 只往早里改）。
    pub(super) async fn settle_order(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        order: &mut StoredOrder,
        facts: &ExchangeFacts<'_>,
        now: DateTime<Utc>,
    ) -> Result<bool, RuntimeError> {
        let exchange_view = facts.view;
        let unbooked = unbooked_quantity(
            exchange_view.cumulative_filled,
            order.record.cumulative_filled,
        );
        let booked = !unbooked.is_zero()
            && !strategy
                .apply_fill(
                    state,
                    &mut order.record,
                    exchange_view.cumulative_filled,
                    facts.observed_at,
                )
                .is_duplicate();

        // 交易所的订单编号要记下来：它证明这张单落过地。下单超时的单这时状态还是「结果
        // 未知」，不记下来就会被当成「从没生效」按期限作废、重新下单（第 9.2 节）。
        let changed = if exchange_view.status.is_working() {
            Self::adopt_snapshot(order, exchange_view, now)
        } else {
            let changed = order.record.status != exchange_view.status
                || order.exchange_order_id.as_deref()
                    != Some(exchange_view.exchange_order_id.as_str());
            order.exchange_order_id = Some(exchange_view.exchange_order_id.clone());
            order.record.status = exchange_view.status;
            changed
        };
        let settled = fills_settled(
            order.record.cumulative_filled,
            exchange_view.cumulative_filled,
        );
        let reconciled_changed = order.fills_reconciled != settled;
        order.fills_reconciled = settled;
        if booked || changed || reconciled_changed {
            order.updated_at = now;
        }
        if booked {
            // 格子状态和订单记录写在同一个事务里。
            self.store
                .save_applied_fills(strategy_id, state, &[&*order])?;
        } else if changed || reconciled_changed {
            self.store.update_order(strategy_id, order)?;
        }
        Ok(booked)
    }

    /// 按交易所给的订单状态更新本地记录，返回是否有变化。
    pub(super) fn adopt_snapshot(
        order: &mut StoredOrder,
        exchange_view: &ExchangeOrder,
        now: DateTime<Utc>,
    ) -> bool {
        let status = if order.record.remaining().is_zero() {
            OrderStatus::Filled
        } else {
            exchange_view.status
        };
        let changed = status != order.record.status
            || order.exchange_order_id.as_deref() != Some(exchange_view.exchange_order_id.as_str());
        order.exchange_order_id = Some(exchange_view.exchange_order_id.clone());
        order.record.status = status;
        if changed {
            order.updated_at = now;
        }
        changed
    }

    /// 拉成交流水的起点（第 9.3 节）。
    ///
    /// 流水不参与记账（累计成交量以订单状态为准），只用来存审计记录、把限速的第一笔
    /// 成交时刻改准。所以起点只是上次同步点往前多查一段，免得时间戳早于同步点的流水
    /// 漏掉；最多往前 [`FILL_STREAM_MAX_WINDOW`]：Hyperliquid 的成交接口按整个账户返回、
    /// 只留最近一万笔，停机很久之后从停机那一刻查起，会一直查不全。
    ///
    /// 只有交易所从没确认过的订单（结果未知、已记录待发送）把起点钉在它挂出的时刻：
    /// 判定它「没有任何影响」要证明从发出到现在的流水里没有它（第 9.2 节）。这类单几十秒
    /// 内就有结论，钉不了多久。挂了很久的在场订单不钉：它的成交量从挂单列表就能读到。
    pub(super) fn fills_since(
        &self,
        stored: &StoredStrategy,
        orders: &[StoredOrder],
    ) -> DateTime<Utc> {
        let watermark = (stored.fills_synced_through - self.options.fills_lookback)
            .max(self.clock.now() - FILL_STREAM_MAX_WINDOW);
        orders
            .iter()
            .filter(|order| outcome_unknown(order.record.status))
            .map(|order| order.placed_at)
            .min()
            .map_or(watermark, |earliest| watermark.min(earliest))
    }

    /// 补拉成交并按 `cloid` 记账。
    ///
    /// 是否记账只看订单的累计成交量（第 9.3 节），不看这条成交流水是否已经存在：
    /// 流水先写、状态后写，中间崩溃时成交不会被吞掉，重复拉到的成交也不会记两次。
    /// 状态和订单记录在同一个事务里落盘，所以不会出现「格子记了、订单没记」的中间态。
    pub(super) async fn sync_fills(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<usize, RuntimeError> {
        let fills = self
            .exchange
            .fills_since(strategy.spec().instrument(), since)
            .await?;
        let mut applied = 0;
        let mut dirty = Vec::new();
        for fill in &fills {
            // 不属于本策略的成交（例如用户手动交易）不记账。
            let Some(index) = orders
                .iter()
                .position(|order| order.record.client_order_id == fill.client_order_id)
            else {
                continue;
            };
            // 成交流水是审计记录：同一笔成交（同一 `trade_id`）只留一条。
            self.store.insert_fill(&StoredFill {
                trade_id: fill.trade_id.clone(),
                strategy_id: strategy_id.clone(),
                client_order_id: fill.client_order_id.clone(),
                price: fill.price,
                quantity: fill.quantity,
                cumulative_quantity: fill.cumulative_quantity,
                filled_at: fill.filled_at,
            })?;

            let order = &mut orders[index];
            // 成交时刻以真实流水为准：回执没有时间戳时先用估计值兜底，这里把它改回
            // 成交真正发生的时刻。这一步不能等到下面「累计量变大」的分支里做——
            // 挂单状态往往先到，等流水到手时累计量已经没有变化了（第 4.1 节）。
            // 重复和乱序的流水不会改（见 `record_fill_time`）。
            if strategy.record_fill_time(state, order.record.unit, fill.filled_at)
                && !dirty.contains(&index)
            {
                dirty.push(index);
            }
            // 累计成交量没有变大：重复、乱序或累计回退的成交，不改变状态。
            if fill.cumulative_quantity <= order.record.cumulative_filled {
                continue;
            }
            if strategy
                .apply_fill(
                    state,
                    &mut order.record,
                    fill.cumulative_quantity,
                    fill.filled_at,
                )
                .is_duplicate()
            {
                continue;
            }
            applied += 1;
            order.updated_at = now;
            // 流水报的累计量比我们记的多，说明上一次拿到的订单状态已经旧了：这张单要回到
            // 订单核对里，按交易所现在的说法重新判定，不沿用旧的「已对账」。
            order.fills_reconciled = false;
            if !dirty.contains(&index) {
                dirty.push(index);
            }
        }

        if !dirty.is_empty() {
            let orders: Vec<&StoredOrder> = dirty.iter().map(|index| &orders[*index]).collect();
            self.store.save_applied_fills(strategy_id, state, &orders)?;
        }
        Ok(applied)
    }

    /// 撤销一张订单，返回它是否已经可以判定为终态。
    ///
    /// `Ok(false)` 表示结果不确定：交易所没有明确确认撤销，订单可能还在、可能刚成交而
    /// 成交记录还没到、也可能压根没落地。这时必须保留它的预留，不能凭「查不到」或者
    /// 「过了 `expiresAfter`」就断定它没有影响（第 9.2 节）。
    pub(super) async fn cancel_order(
        &self,
        instrument: &Instrument,
        order: &mut StoredOrder,
        now: DateTime<Utc>,
    ) -> Result<bool, RuntimeError> {
        let client_order_id = order.record.client_order_id.clone();
        self.count_action();
        let outcome = self
            .exchange
            .cancel_order(instrument, &client_order_id)
            .await;
        match outcome {
            Ok(()) => {
                order.record.status = terminal_status(&order.record);
                // 撤销只说明订单不再在场：撤销前可能已经有成交、成交记录也可能还没到，
                // 交易所给的最终累计成交量还不知道。核对没有完成，回查起点要继续保留，
                // 规划也要等对完账再按新的状态重算数量（第 9.2 节）。
                order.fills_reconciled = false;
                Ok(true)
            }
            // 交易所不认识这张单：它可能已经成交、可能早就撤销、也可能压根没落地。
            // 「查不到」分不出是哪一种，成交记录也可能还没到，所以即使过了
            // `expiresAfter` 也保留预留、不释放；接下来的对账会按 `cloid` 查它，查不到就
            // 记成结果未知，期限过后由三项核对决定（第 9.2 节）。
            Err(ExchangeError::UnknownOrder { .. }) => {
                tracing::warn!(
                    client_order_id = %client_order_id,
                    expired = now >= order.expires_at,
                    "the exchange does not know this order, keeping the reservation"
                );
                Ok(false)
            }
            Err(ExchangeError::Rejected { reason }) => {
                tracing::warn!(
                    client_order_id = %client_order_id,
                    reason = %reason,
                    "cancel rejected"
                );
                Err(ExchangeError::Rejected { reason }.into())
            }
            // 操作次数超限：撤销没有发生，订单还在场。报给调用方暂停策略（B30）。
            Err(ExchangeError::RateLimited { reason }) => {
                tracing::error!(
                    client_order_id = %client_order_id,
                    reason = %reason,
                    "cancel rejected for too many requests"
                );
                Err(ExchangeError::RateLimited { reason }.into())
            }
            Err(error) => {
                tracing::warn!(
                    client_order_id = %client_order_id,
                    error = %error,
                    "cancel result unknown, keeping the reservation"
                );
                Ok(false)
            }
        }
    }
}

/// 订单不再在场时的终态：全部成交记成交，其余记撤销。
pub(super) fn terminal_status(record: &OrderRecord) -> OrderStatus {
    if record.remaining().is_zero() {
        OrderStatus::Filled
    } else {
        OrderStatus::Canceled
    }
}
