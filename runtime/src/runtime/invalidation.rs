//! 观点失效（第 7.5 节）：标记价越过失效价之后撤单、用只减仓单减到剩余数量，所有订单
//! 都有了结论、仓位不超过剩余数量之后进入「已失效」。
//!
//! 失效中的每一步都按同一个顺序走：
//! 1. 撤掉全部在场订单、按 `cloid` 核对终态、补记成交（[`Runtime::cancel_only_round`]）；
//! 2. 读实时仓位和标记价；成交同步走通时顺带做仓位核对，用来作废结果未知的订单；
//! 3. 补发一张立即成交否则撤销的只减仓单（[`Strategy::reduction`]）；
//! 4. 全部订单有了结论、仓位不超过剩余数量时转「已失效」。
//!
//! 减仓数量按交易所实时查询的仓位算，所以成交推送断开、成交同步失败时照常执行（B39）。
//! 行情过期时不发单：限价是按标记价算的，旧的标记价算不出该让多少。

use chrono::{DateTime, Utc};
use poise_strategy::Strategy;
use poise_strategy::decision::{Permits, PositionCheck};
use poise_strategy::invalidation::Reduction;
use poise_strategy::order::{ClientOrderId, WorkingOrder};
use poise_strategy::planning::Plan;
use poise_strategy::state::StrategyState;
use poise_strategy::units::{Notional, Position, Price};

use crate::store::{StoredOrder, StrategyId, StrategyStatus};

use super::cancel_only::{cancel_only_note, cancel_only_settled};
use super::step::{Placement, expected_position};
use super::{NewOrders, Runtime, RuntimeError, StepReport, fresh};

/// 失效中一步的读数。
struct Readings {
    market_price: Price,
    priced_at: DateTime<Utc>,
    position: Position,
    expected_position: Position,
    /// 可用资金只进视图快照；没读到时视图只改说明。
    available_collateral: Option<Notional>,
    /// 仓位核对的结论；成交同步没走通时没有，结果未知的订单这一步就不作废。
    check: Option<PositionCheck>,
}

impl Runtime {
    /// 运行中的策略先看失效价：标记价越过失效价就落「失效中」，当场走一步失效。
    ///
    /// 没设失效价的策略不多读一次行情。行情读不到、或者读回来已经过期时这一步不判定，
    /// 交给常规的一步：它同样要读行情，读不到时这一步本来就不下新单。
    pub(super) async fn check_invalidation(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        now: DateTime<Utc>,
    ) -> Result<Option<StepReport>, RuntimeError> {
        if strategy
            .sides()
            .iter()
            .all(|side| side.config().invalidation.is_none())
        {
            return Ok(None);
        }
        let settings = self.store.load_account_settings()?;
        let priced_at = self.clock.now();
        let mark = match self.exchange.mark_price(strategy.spec().instrument()).await {
            Ok(mark) => mark,
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "no mark price to check the invalidation price");
                return Ok(None);
            }
        };
        if !fresh(priced_at, self.clock.now(), settings.market_data_max_age) {
            return Ok(None);
        }
        self.invalidate_if_crossed(strategy_id, strategy, mark, now)
            .await
    }

    /// 标记价越过失效价就落「失效中」，当场走一步失效；没越过返回 `None`。
    ///
    /// 调用方负责确认这份标记价是新鲜的。运行中的一步会看两次：开头单独读一次（读取阶段
    /// 失败也不耽误止损），常规读取拿到新的标记价之后再看一次（开头那次可能没读到，或者
    /// 那时还没越过）。
    pub(super) async fn invalidate_if_crossed(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        mark: Price,
        now: DateTime<Utc>,
    ) -> Result<Option<StepReport>, RuntimeError> {
        let Some(direction) = strategy.invalidation_triggered(mark) else {
            return Ok(None);
        };
        tracing::warn!(
            strategy = %strategy_id,
            %direction,
            %mark,
            "the mark price crossed the invalidation price, reducing the position"
        );
        // 先落「失效中」：从这里开始只撤单、只减仓，进程崩了重启之后也是。
        self.store
            .save_strategy_status(strategy_id, StrategyStatus::Invalidating, now)?;
        self.invalidation_step(strategy_id, now).await.map(Some)
    }

    /// 失效中的策略走一步（见模块说明）。
    ///
    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    pub(super) async fn invalidation_step(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        if stored.status != StrategyStatus::Invalidating {
            return Err(RuntimeError::NotRunning(strategy_id.clone(), stored.status));
        }
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let mut state = self.load_state(strategy_id, &strategy)?;
        let mut orders = self.store.load_orders(strategy_id)?;
        let settings = self.store.load_account_settings()?;

        // 1. 撤单：增仓单、格子单、上一步没成交完的减仓单一律撤掉。同步失败也照撤。
        let round = self
            .cancel_only_round(
                strategy_id,
                &stored,
                &strategy,
                &mut state,
                &mut orders,
                now,
            )
            .await?;

        // 2. 读数。
        let readings = match self
            .invalidation_readings(strategy_id, &strategy, &mut state, &mut orders, &round, now)
            .await
        {
            Ok(readings) => readings,
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "no position or mark price, retrying the reduction next step");
                let note = format!(
                    "失效减仓中：这一步没读到仓位或行情（{error}），下一轮再试。{}",
                    cancel_only_note(&orders, StrategyStatus::Invalidating, None)
                        .unwrap_or_default()
                );
                self.annotate_view(strategy_id, StrategyStatus::Invalidating, note)
                    .await?;
                let mut report = self.nothing_happened(strategy_id, StrategyStatus::Invalidating);
                report.canceled = round.canceled;
                report.applied_fills = round.applied_fills;
                return Ok(report);
            }
        };
        let unknown = match readings.check {
            Some(check) => {
                Some(self.settle_unknown_orders(strategy_id, &mut orders, check, now)?)
            }
            None => None,
        };

        // 3. 补发减仓单：数量扣掉还可能成交的减仓单（含结果未知的，B39c）。
        let working: Vec<WorkingOrder> = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.record.working_view())
            .collect();
        let reduction = strategy.reduction(readings.position, readings.market_price, &working);
        let new_orders = NewOrders {
            permits: Permits::ALL,
            priced_at: readings.priced_at,
            max_age: settings.market_data_max_age,
        };
        let mut placed: Vec<ClientOrderId> = Vec::new();
        let mut stale = false;
        if let Some(intent) = &reduction.order {
            match self
                .place_one(
                    strategy_id,
                    &strategy,
                    &mut state,
                    &mut orders,
                    intent,
                    &new_orders,
                )
                .await?
            {
                // 失效中不因为操作次数超限暂停：止损要继续，超限后交易所仍然每 10 秒放行一次，
                // 下一步照常重试。
                Placement::Sent(client_order_id)
                | Placement::Rejected(client_order_id)
                | Placement::MarginRejected(client_order_id)
                | Placement::RateLimited(client_order_id) => placed.push(client_order_id),
                Placement::NotSent => {}
                Placement::StaleMarket => stale = true,
            }
        }
        // 同步点由撤单那一轮推进（只在补拉流水走通时，见 [`Runtime::cancel_only_round`]）。

        // 4. 收场：撤单和核对都走通，订单全部有了结论、成交对完账，仓位不超过剩余数量（或
        //    只剩下不了单的尾量）。这一步刚发过减仓单就等下一步：它的成交还要核对。
        // 「超出部分不足最低金额」是按标记价判断的：行情过期时这个结论不算数，留在失效中
        // 下一步重读（第 7.2 节）。
        if !fresh(
            readings.priced_at,
            self.clock.now(),
            settings.market_data_max_age,
        ) {
            stale = true;
        }
        let done = round.is_clean()
            && placed.is_empty()
            && !stale
            && reduction.settled
            && cancel_only_settled(&orders);
        let status = if done {
            self.store
                // 收场时刻按此刻的时钟记，理由同停止归档（资金费按它截止）。
                .save_strategy_status(strategy_id, StrategyStatus::Invalidated, self.clock.now())?;
            tracing::warn!(
                strategy = %strategy_id,
                position = %readings.position,
                tail = %reduction.excess,
                "invalidation finished"
            );
            StrategyStatus::Invalidated
        } else {
            StrategyStatus::Invalidating
        };

        let report = StepReport {
            strategy_id: strategy_id.clone(),
            status,
            plan: Plan {
                market_price: readings.market_price,
                place: reduction.order.iter().cloned().collect(),
                ..Plan::default()
            },
            placed,
            canceled: round.canceled,
            applied_fills: round.applied_fills,
            position: readings.position,
            expected_position: readings.expected_position,
        };
        let note = invalidation_note(
            status,
            &reduction,
            !report.placed.is_empty(),
            stale,
            cancel_only_note(&orders, StrategyStatus::Invalidating, unknown.as_ref()),
        );
        match readings.available_collateral {
            Some(available) => {
                // 保证金率只进视图快照：失效中只发只减仓单，闸门管不着它。
                let margin_ratio = self
                    .margin_ratio(strategy_id, settings.market_data_max_age)
                    .await
                    .map(|(ratio, _)| ratio);
                self.save_view(
                    strategy_id,
                    &report,
                    available,
                    margin_ratio,
                    Some(note),
                    now,
                )
                .await?
            }
            None => self.annotate_view(strategy_id, status, note).await?,
        }
        Ok(report)
    }

    /// 失效中一步的读数。成交同步走通时走常规的读数（带仓位核对）；没走通时直接问交易所
    /// 要仓位和标记价——减仓按实时仓位算，不能因为成交流水断了就停下来（B39）。
    async fn invalidation_readings(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
        round: &super::CancelOnlyRound,
        now: DateTime<Utc>,
    ) -> Result<Readings, RuntimeError> {
        if let Ok(synced) = &round.synced
            && round.cancel_error.is_none()
        {
            match self
                .reading_step(strategy_id, strategy, state, orders, synced, now)
                .await
            {
                Ok(reading) => {
                    return Ok(Readings {
                        market_price: reading.market_price,
                        priced_at: reading.priced_at,
                        position: reading.reading.actual,
                        expected_position: reading.reading.expected,
                        available_collateral: Some(reading.available_collateral),
                        check: Some(reading.check),
                    });
                }
                Err(error) => {
                    // 核对循环里会再记账、再写库：失败时丢掉内存，按库里的那份接着走。
                    tracing::warn!(strategy = %strategy_id, %error, "the regular readings failed, asking for the position directly");
                    *state = self.load_state(strategy_id, strategy)?;
                    *orders = self.store.load_orders(strategy_id)?;
                }
            }
        }
        let instrument = strategy.spec().instrument();
        let priced_at = self.clock.now();
        let market_price = self.exchange.mark_price(instrument).await?;
        let position = self.exchange.position(instrument).await?;
        Ok(Readings {
            market_price,
            priced_at,
            position,
            expected_position: expected_position(strategy, state, &[], orders),
            available_collateral: self.account.available_collateral().await.ok(),
            check: None,
        })
    }
}

/// 失效中、已失效写给用户看的说明。
fn invalidation_note(
    status: StrategyStatus,
    reduction: &Reduction,
    placed: bool,
    stale: bool,
    progress: Option<String>,
) -> String {
    let tail = (reduction.settled && !reduction.excess.is_zero()).then(|| {
        format!(
            "超出剩余数量的 {} 不足最低下单金额，下不了单，由你决定是否手动处理。",
            reduction.excess
        )
    });
    let head = if status == StrategyStatus::Invalidated {
        "已失效：标记价越过了失效价，订单都已撤销、仓位已减到剩余数量。价格回来也不会自动恢复；\
         要继续做，新建一个策略接管剩下的仓位。"
            .to_string()
    } else if reduction.settled {
        "失效减仓中：仓位已经减到剩余数量，等所有订单都有结论之后转为已失效。".to_string()
    } else if placed {
        // 这一步刚发出减仓单：超出量是发单之前的读数，成交多少下一步核对之后才知道。
        format!(
            "失效减仓中：标记价越过了失效价，已发出只减仓单（超出剩余数量 {}），下一步核对成交。",
            reduction.excess
        )
    } else {
        format!(
            "失效减仓中：标记价越过了失效价，正在用只减仓单减仓，还要减 {}。",
            reduction.excess
        )
    };
    let stale = stale
        .then(|| "这一步的行情过期了，按它算出来的减仓不算数，下一步按新行情再算。".to_string());
    [
        Some(head),
        stale,
        tail,
        progress.filter(|_| status == StrategyStatus::Invalidating),
    ]
    .into_iter()
    .flatten()
    .collect()
}
