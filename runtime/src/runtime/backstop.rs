//! 交易所侧的兜底止损单（第 7.5 节的补充）。
//!
//! 观点失效靠 poise 每一步看标记价：进程停了、电脑睡了、断网了，失效价就没人看。兜底单是
//! 挂在 Hyperliquid 上的只减仓止损市价单，触发价在失效价外再 1%
//! （[`poise_strategy::invalidation::BACKSTOP_DISTANCE`]），数量是本侧仓位超出剩余数量 R
//! 的部分。poise 在线时失效减仓先走，兜底单几乎不会被碰到；poise 不在时由交易所执行。
//!
//! 兜底单不进订单账本：它不属于任何批次或格子，对账只认库里的订单，交易所上多出来的这张单
//! 和它的成交都会被忽略。它单独记在 `backstops` 表里，每一步之前维护一遍
//! （[`Runtime::maintain_backstops`]）：
//! - 运行中、恢复中、暂停中、已暂停：按实时仓位挂着；仓位、失效价、R 变了就撤了重挂。
//!   暂停也留着——暂停只是先别做网格，失效价照样算数。
//! - 停止中、失效中、平仓中：撤掉（[`Runtime::clear_backstops`]，在那几个状态各自的一步
//!   开头，减仓单发出之前）。归档之前必须撤干净。
//!
//! 兜底单成交过（poise 掉线期间触发、或者价格一下穿过了失效价和触发价），说明观点已经失效：
//! 转「失效中」，由常规的失效流程撤掉剩下的挂单、按实时仓位收尾。

use chrono::{DateTime, Utc};
use poise_strategy::Strategy;
use poise_strategy::invalidation::Backstop;
use poise_strategy::order::OrderStatus;
use poise_strategy::units::{Price, Quantity};
use serde::{Deserialize, Serialize};

use crate::exchange::{ExchangeError, OrderLookup, StopRequest};
use crate::store::{StoredBackstop, StoredStrategy, StrategyId, StrategyStatus};

use super::step::new_client_order_id;
use super::{Runtime, RuntimeError, StepReport};

/// 详情页上的一张兜底单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackstopView {
    pub direction: poise_strategy::side::Direction,
    pub trigger_price: Price,
    pub quantity: Quantity,
    /// 交易所确认挂上了；没确认的还在核对。
    pub confirmed: bool,
}

/// 一张兜底单这一轮查到的结局。
enum Seen {
    /// 还挂着。
    Live,
    /// 成交过：观点已经失效。
    Fired,
    /// 已经不在交易所上（撤掉、拒绝、确认没挂上）：记录可以删。
    Gone,
    /// 这一轮说不清（查询失败、结果未知还没过期限）。
    Unsettled,
}

impl Runtime {
    /// 驱动一步之前维护兜底单（见模块说明）。兜底单成交过时转「失效中」并当场走一步失效，
    /// 返回那一步的报告；否则返回 `None`，照常走这个状态的一步。
    ///
    /// 交易所查询失败只记日志、留到下一步：兜底单维护不能挡住这一步的撤单和止损。
    pub(super) async fn maintain_backstops(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        now: DateTime<Utc>,
    ) -> Result<Option<StepReport>, RuntimeError> {
        // 停止中、失效中、平仓中的兜底单由那一步自己撤（`cancel_only_step`、
        // `reduction_step`）：网页和命令行的停止不经过这里。
        let keeps = matches!(
            stored.status,
            StrategyStatus::Running
                | StrategyStatus::Restoring
                | StrategyStatus::Pausing
                | StrategyStatus::Paused
        );
        if keeps && self.keep_backstops(strategy_id, stored, now).await? {
            self.store
                .save_strategy_status(strategy_id, StrategyStatus::Invalidating, now)?;
            return self.invalidation_step(strategy_id, now).await.map(Some);
        }
        Ok(None)
    }

    /// 详情页上的兜底单。
    pub(super) fn backstop_views(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Vec<BackstopView>, RuntimeError> {
        Ok(self
            .store
            .load_backstops(strategy_id)?
            .into_iter()
            .map(|backstop| BackstopView {
                direction: backstop.direction,
                trigger_price: backstop.trigger_price,
                quantity: backstop.quantity,
                confirmed: backstop.confirmed,
            })
            .collect())
    }

    /// 页面上的提醒：兜底单最近一次挂不上的原因。
    pub(super) fn backstop_note(&self, strategy_id: &StrategyId) -> Option<String> {
        self.backstop_problems
            .lock()
            .expect("backstop lock poisoned")
            .get(strategy_id)
            .map(|reason| format!("交易所兜底止损单挂不上（{reason}），poise 离线时没有止损。"))
    }

    /// 挂着兜底单：先看已有的结局，再按实时仓位对齐。返回兜底单是否成交过。
    async fn keep_backstops(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        now: DateTime<Utc>,
    ) -> Result<bool, RuntimeError> {
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let backstops = self.store.load_backstops(strategy_id)?;
        // 没设失效价、交易所上也没有兜底单：什么都不用查。
        if backstops.is_empty()
            && strategy
                .sides()
                .iter()
                .all(|side| side.config().invalidation.is_none())
        {
            return Ok(false);
        }
        let mut live = Vec::new();
        let mut unsettled = false;
        let mut fired = false;
        for backstop in backstops {
            match self
                .look_up_backstop(strategy_id, stored, &backstop, now)
                .await?
            {
                Seen::Live => live.push(backstop),
                Seen::Fired => fired = true,
                Seen::Gone => {}
                Seen::Unsettled => unsettled = true,
            }
        }
        if fired {
            return Ok(true);
        }
        // 有一张说不清的单时不动：再挂一张可能就是两张。
        if unsettled {
            return Ok(false);
        }
        let position = match self.exchange.position(&stored.instrument).await {
            Ok(position) => position,
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "no position to size the backstop");
                return Ok(false);
            }
        };
        let wanted = strategy.backstops(position);

        // 和要的对不上的撤掉：撤成了才在同一侧挂新的。
        let mut kept: Vec<StoredBackstop> = Vec::new();
        for backstop in live {
            if wanted.iter().any(|want| same(want, &backstop)) {
                kept.push(backstop);
                continue;
            }
            if !self.cancel_backstop(strategy_id, stored, &backstop).await? {
                kept.push(backstop);
            }
        }
        let missing: Vec<&Backstop> = wanted
            .iter()
            .filter(|want| {
                !kept
                    .iter()
                    .any(|backstop| backstop.direction == want.direction)
            })
            .collect();
        if missing.is_empty() {
            self.set_backstop_problem(strategy_id, None);
            return Ok(false);
        }
        // 价格已经越过失效价时不挂：挂上去会立即按市价成交（测试网实测），抢在 poise 自己
        // 按滑点上限减仓之前。这一步的失效检查会接手。读不到标记价也先不挂。
        let mark = match self.exchange.mark_price(&stored.instrument).await {
            Ok(mark) => mark,
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "no mark price to place the backstop");
                return Ok(false);
            }
        };
        if strategy.invalidation_triggered(mark).is_some() {
            return Ok(false);
        }
        let mut problem = None;
        for want in missing {
            if let Err(reason) = self.place_backstop(strategy_id, stored, want, now).await? {
                problem = Some(reason);
            }
        }
        self.set_backstop_problem(strategy_id, problem);
        Ok(false)
    }

    /// 撤掉全部兜底单：停止中、失效中、平仓中。
    pub(super) async fn clear_backstops(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        now: DateTime<Utc>,
    ) -> Result<(), RuntimeError> {
        self.set_backstop_problem(strategy_id, None);
        for backstop in self.store.load_backstops(strategy_id)? {
            if !self.cancel_backstop(strategy_id, stored, &backstop).await? {
                // 撤不掉（多半是已经触发或者早就没了）：按 `cloid` 查一下结局。
                self.look_up_backstop(strategy_id, stored, &backstop, now)
                    .await?;
            }
        }
        Ok(())
    }

    /// 按 `cloid` 查一张兜底单的结局，结局已定的删掉记录。成交过的发提醒。
    async fn look_up_backstop(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        backstop: &StoredBackstop,
        now: DateTime<Utc>,
    ) -> Result<Seen, RuntimeError> {
        let lookup = match self
            .exchange
            .order_status(&stored.instrument, &backstop.client_order_id)
            .await
        {
            Ok(lookup) => lookup,
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "cannot look up the backstop");
                return Ok(Seen::Unsettled);
            }
        };
        let seen = match lookup {
            OrderLookup::Found(order) if !order.cumulative_filled.is_zero() => Seen::Fired,
            OrderLookup::Found(order) => match order.status {
                OrderStatus::Filled => Seen::Fired,
                OrderStatus::Canceled | OrderStatus::Rejected => Seen::Gone,
                _ => {
                    if !backstop.confirmed {
                        self.store.save_backstop(
                            strategy_id,
                            &StoredBackstop {
                                confirmed: true,
                                ..backstop.clone()
                            },
                        )?;
                    }
                    Seen::Live
                }
            },
            // 交易所不认识：还没过期限就可能还在路上；过了期限加余量还查不到，就是没挂上。
            OrderLookup::Unknown
                if now > backstop.expires_at + self.options.unknown_order_grace =>
            {
                Seen::Gone
            }
            OrderLookup::Unknown => Seen::Unsettled,
        };
        match seen {
            Seen::Fired => {
                self.store.delete_backstop(&backstop.client_order_id)?;
                tracing::warn!(
                    strategy = %strategy_id,
                    trigger = %backstop.trigger_price,
                    "the backstop on the exchange was triggered"
                );
                self.notify_strategy(
                    strategy_id,
                    "backstop-fired",
                    "兜底单已触发",
                    format!(
                        "标记价越过兜底触发价 {}，交易所已按市价减仓。",
                        backstop.trigger_price
                    ),
                );
            }
            Seen::Gone => self.store.delete_backstop(&backstop.client_order_id)?,
            Seen::Live | Seen::Unsettled => {}
        }
        Ok(seen)
    }

    /// 撤一张兜底单。撤成了删掉记录，返回 `true`；交易所说没有这张单、或者撤销失败，留着
    /// 记录交给下一次查询，返回 `false`。
    async fn cancel_backstop(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        backstop: &StoredBackstop,
    ) -> Result<bool, RuntimeError> {
        self.count_action();
        match self
            .exchange
            .cancel_order(&stored.instrument, &backstop.client_order_id)
            .await
        {
            Ok(()) => {
                self.store.delete_backstop(&backstop.client_order_id)?;
                Ok(true)
            }
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "cannot cancel the backstop");
                Ok(false)
            }
        }
    }

    /// 挂一张兜底单：先写库、再发送。交易所明确拒绝时删掉记录，返回原因。
    async fn place_backstop(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        want: &Backstop,
        now: DateTime<Utc>,
    ) -> Result<Result<(), String>, RuntimeError> {
        let backstop = StoredBackstop {
            client_order_id: new_client_order_id(),
            direction: want.direction,
            trigger_price: want.trigger_price,
            limit_price: want.limit_price,
            quantity: want.quantity,
            placed_at: now,
            expires_at: self.clock.now() + self.options.order_expires_after,
            confirmed: false,
        };
        self.store.save_backstop(strategy_id, &backstop)?;
        self.count_action();
        let request = StopRequest {
            instrument: stored.instrument.clone(),
            client_order_id: backstop.client_order_id.clone(),
            side: want.side,
            trigger_price: want.trigger_price,
            limit_price: want.limit_price,
            quantity: want.quantity,
            expires_at: backstop.expires_at,
        };
        match self.exchange.place_stop(&request).await {
            Ok(_) => {
                self.store.save_backstop(
                    strategy_id,
                    &StoredBackstop {
                        confirmed: true,
                        ..backstop
                    },
                )?;
                tracing::info!(
                    strategy = %strategy_id,
                    trigger = %want.trigger_price,
                    quantity = %want.quantity,
                    "backstop placed"
                );
                Ok(Ok(()))
            }
            Err(
                error @ (ExchangeError::Rejected { .. }
                | ExchangeError::InsufficientMargin { .. }
                | ExchangeError::RateLimited { .. }),
            ) => {
                self.store.delete_backstop(&backstop.client_order_id)?;
                tracing::warn!(strategy = %strategy_id, %error, "the backstop was refused");
                Ok(Err(error.to_string()))
            }
            // 结果未知：记录留着，下一步按 `cloid` 查。
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "the backstop placement is unknown");
                Ok(Ok(()))
            }
        }
    }

    /// 记下（或清掉）兜底单挂不上的原因；新出现的原因提醒一次。
    fn set_backstop_problem(&self, strategy_id: &StrategyId, problem: Option<String>) {
        let previous = {
            let mut problems = self
                .backstop_problems
                .lock()
                .expect("backstop lock poisoned");
            match &problem {
                Some(reason) => problems.insert(strategy_id.clone(), reason.clone()),
                None => problems.remove(strategy_id),
            }
        };
        if let Some(reason) = problem
            && previous.as_ref() != Some(&reason)
        {
            self.notify_strategy(
                strategy_id,
                "backstop-refused",
                "兜底单挂不上",
                format!("{reason}。poise 离线时这个策略没有止损。"),
            );
        }
    }
}

/// 已挂的兜底单和要的是不是同一张：同一侧、同一触发价、同一数量。
fn same(want: &Backstop, backstop: &StoredBackstop) -> bool {
    want.direction == backstop.direction
        && want.trigger_price == backstop.trigger_price
        && want.quantity == backstop.quantity
}
