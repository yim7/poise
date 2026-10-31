//! 运行中的一步：读取、判定、执行（第 4、7、9 节）。

use chrono::{DateTime, Duration, Utc};
use poise_strategy::Strategy;
use poise_strategy::decision::{
    MarginState, PauseReason, Permits, PositionCheck, PositionReading, SystemState,
    expected_position as expected_from, judge, pending_units, position_check,
};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderRecord, OrderStatus, WorkingOrder};
use poise_strategy::planning::{OrderIntent, PendingUnit, Plan, WaitReason};
use poise_strategy::state::StrategyState;
use poise_strategy::units::{Position, Price, Quantity, Rate};

use crate::exchange::{ExchangeError, OrderRequest, TimeInForce};
use crate::settings::AccountSettings;
use crate::store::{StoredOrder, StoredStrategy, StrategyId, StrategyStatus};

use super::cancel_only::cancel_only_note;
use super::funds::FundsCheck;
use super::{
    Cancels, Executed, NewOrders, POSITION_CHECK_ROUNDS, Reading, Runtime, RuntimeError, Snapshot,
    StepReport, Synced, order_states,
};
use super::{DAILY_ACTION_LIMIT, MarginLevel, fresh, percent};
use crate::notify::Notice;

impl Runtime {
    /// 走一步。
    ///
    /// 同一个策略同一时刻只能走一步：并发调用会返回 [`RuntimeError::Busy`]。
    /// 走的是哪条路由策略的状态决定：运行中按常规下单，暂停中/停止中只撤单，
    /// 恢复中先把遗留订单收干净（[`Runtime::drive_inner`]）。
    pub async fn step(&self, strategy_id: &StrategyId) -> Result<StepReport, RuntimeError> {
        let _guard = self.begin(strategy_id)?;
        self.drive_inner(strategy_id, self.clock.now()).await
    }

    /// 按状态驱动一步。
    ///
    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    pub(super) async fn drive_inner(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        // 交易所上的兜底单先对齐：成交过就说明观点已经失效，这一步改走失效（`backstop`）。
        if let Some(report) = self.maintain_backstops(strategy_id, &stored, now).await? {
            return Ok(report);
        }
        // 失效价是用户声明的止损，暂停只是「先别做网格」：已暂停、暂停中、恢复中的策略越过
        // 失效价同样进入失效（第 7.5 节）。停止中不算：停止就是用户决定自己接管仓位。
        if matches!(
            stored.status,
            StrategyStatus::Paused | StrategyStatus::Pausing | StrategyStatus::Restoring
        ) {
            let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
            if let Some(report) = self.check_invalidation(strategy_id, &strategy, now).await? {
                return Ok(report);
            }
        }
        match stored.status {
            status if status.is_cancel_only() => {
                self.cancel_only_step(strategy_id, &stored, now).await
            }
            // 已暂停的策略只看失效价，别的什么都不做。
            StrategyStatus::Paused => {
                Ok(self.nothing_happened(strategy_id, StrategyStatus::Paused))
            }
            StrategyStatus::Restoring => self.restore_step(strategy_id, now).await,
            StrategyStatus::Running => self.running_step(strategy_id, now).await,
            StrategyStatus::Invalidating => self.invalidation_step(strategy_id, now).await,
            StrategyStatus::Closing => self.closing_step(strategy_id, now).await,
            status if status.is_archived() => Err(RuntimeError::Archived(strategy_id.clone())),
            other => Err(RuntimeError::NotRunning(strategy_id.clone(), other)),
        }
    }

    /// 运行中的策略走一步。
    ///
    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    ///
    /// 一步分三段：读取（[`Runtime::read_step`]）、判定（[`judge`]）、执行
    /// （[`Runtime::execute_plan`]）。判定是纯计算，所以「这一步该不该继续下单」
    /// 可以直接用数据测试，不用连交易所。
    ///
    /// 走完落一份视图快照（第 11 节）：网页的列表和详情只读它，不再去问交易所。
    pub(super) async fn running_step(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        if stored.status != StrategyStatus::Running {
            return Err(RuntimeError::NotRunning(strategy_id.clone(), stored.status));
        }
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let mut state = self.load_state(strategy_id, &strategy)?;
        // 失效价最先看：越过了就不再按常规下单，撤单、减仓（第 7.5 节）。
        if let Some(report) = self.check_invalidation(strategy_id, &strategy, now).await? {
            return Ok(report);
        }
        let mut orders = self.store.load_orders(strategy_id)?;
        let settings = self.store.load_account_settings()?;

        // 保证金率最先读：超过停止阈值时增仓单必须撤掉，后面哪一项读取失败都不能让它们
        // 留在场上（第 7.4 节）。读到的时刻也是这一步读数的起点：发新单前按它判断读数有没有
        // 过期（第 7.2 节）。
        let reads_started_at = self.clock.now();
        let margin_reading = self
            .margin_ratio(strategy_id, settings.market_data_max_age)
            .await;
        let margin_ratio = margin_reading.map(|(ratio, _)| ratio);
        let margin = margin_state(margin_ratio, &settings);
        let (withdrawn, withdraw_rate_limited) = if margin == MarginState::AboveStop {
            self.withdraw_increasing_now(strategy_id, &strategy, &mut orders, now)
                .await?
        } else {
            (Vec::new(), false)
        };
        // 提前撤单就被以操作次数超限拒绝：后面的读取和规划不做了，直接暂停（B30）。暂停的
        // 撤单轮会把每张单都再试一遍。
        if withdraw_rate_limited {
            self.store
                .save_pause_reason(strategy_id, RATE_LIMITED_NOTE)?;
            self.notify_strategy(
                strategy_id,
                "paused",
                "已暂停",
                RATE_LIMITED_NOTE.to_string(),
            );
            let (status, round) = self
                .pause_inner(strategy_id, &strategy, &mut state, &mut orders, now)
                .await?;
            let mut report = self.nothing_happened(strategy_id, status);
            report.canceled = withdrawn.into_iter().chain(round.canceled).collect();
            report.applied_fills = round.applied_fills;
            let note = join_notes([
                Some(RATE_LIMITED_NOTE.to_string()),
                cancel_only_note(&orders, status, None),
            ])
            .unwrap_or_default();
            self.annotate_view(strategy_id, status, note).await?;
            return Ok(report);
        }

        let snapshot = match self
            .read_step(
                strategy_id,
                &stored,
                &strategy,
                &mut state,
                &mut orders,
                settings.market_data_max_age,
                now,
            )
            .await
        {
            Ok(Read::Snapshot(snapshot)) => snapshot,
            // 常规读取读到的标记价越过了失效价：后面的读取都不做，当场进入失效（第 7.5 节）。
            Ok(Read::Crossed(mark)) => {
                let mut report = self
                    .invalidate_if_crossed(strategy_id, &strategy, mark, now)
                    .await?
                    .expect("read_step only reports a crossing it has checked");
                report.canceled.splice(0..0, withdrawn);
                return Ok(report);
            }
            Err(error) => {
                // 读取阶段没走通，可能连标记价都没读到：再单独看一次失效价，越过了就当场进入
                // 失效。别的读数失败不能耽误止损（第 7.5 节）。
                if let Some(mut report) =
                    self.check_invalidation(strategy_id, &strategy, now).await?
                {
                    report.canceled.splice(0..0, withdrawn);
                    return Ok(report);
                }
                // 这一步到此为止：不发新单，已在场的挂单保留（第 7.2 节）。页面只读视图快照，
                // 不写一句的话它会一直显示上一步的「没有等待」，用户看不出策略其实已经好几步
                // 没核对成交、也没下单了（第 11 节）。
                self.annotate_view_waiting(
                    strategy_id,
                    StrategyStatus::Running,
                    format!("没读到成交或订单状态（{error}），这一步不发新单，下一步重新核对。"),
                    vec![WaitReason::FillsPending],
                )
                .await?;
                return Err(error);
            }
        };
        // 结果未知的订单：三项核对都有了就作废、释放预留（第 9.2 节）。放在判定之前：
        // 作废之后，闸门在这一步就能打开。
        let unknown = self.settle_unknown_orders(strategy_id, &mut orders, snapshot.check, now)?;
        let decision = judge(
            snapshot.check,
            snapshot.inconsistent.as_ref(),
            &order_states(&orders),
            SystemState {
                unknown_orders: unknown.any_pending(),
                margin,
            },
        );
        if let Some(reason) = decision.pause {
            let mut report = self
                .pause_for(
                    strategy_id,
                    &strategy,
                    &mut state,
                    &mut orders,
                    &snapshot,
                    reason,
                    now,
                )
                .await?;
            report.canceled.splice(0..0, withdrawn);
            let note = cancel_only_note(&orders, report.status, None);
            self.save_view(
                strategy_id,
                &report,
                snapshot.available_collateral,
                margin_ratio,
                note,
                now,
            )
            .await?;
            return Ok(report);
        }
        if decision.permits != Permits::ALL {
            tracing::info!(
                strategy = %strategy_id,
                waiting = ?decision.waiting,
                actual = %snapshot.reading.actual,
                expected = %snapshot.reading.expected,
                "the gate is closed: no new orders this step, planned cancels still go out"
            );
        }

        let mut plan = self.plan_step(
            &strategy,
            &state,
            &orders,
            &decision.blocked,
            snapshot.market_price,
        );
        for reason in &decision.waiting {
            if !plan.waiting.contains(reason) {
                plan.waiting.push(*reason);
            }
        }
        if decision.withdraw_increasing {
            withdraw_increasing(&mut plan, &orders);
        }
        // 保证金读数的时刻：交易所给的时刻和读到的时刻里更早的那个（第 7.2 节）。
        let margin_read_at = margin_reading
            .and_then(|(_, as_of)| as_of)
            .map_or(reads_started_at, |as_of| as_of.min(reads_started_at));
        let margin_read_at = self
            .refresh_margin_if_stale(strategy_id, margin, margin_read_at, &settings)
            .await;
        let executed = self
            .execute_plan(
                strategy_id,
                &strategy,
                &mut state,
                &mut orders,
                &mut plan,
                NewOrders {
                    permits: decision.permits,
                    // 发新单依据的读数里最旧的那个时刻：保证金读数和标记价。
                    priced_at: margin_read_at.min(snapshot.priced_at),
                    max_age: settings.market_data_max_age,
                },
                now,
            )
            .await;
        let executed = match executed {
            Ok(executed) => executed,
            // 撤单被以操作次数超限拒绝：这一步剩下的撤单、下单都不做，同样暂停。已经撤掉的
            // 单都已逐张落库。
            Err(RuntimeError::Exchange(ExchangeError::RateLimited { .. })) => Executed {
                placed: Vec::new(),
                canceled: Vec::new(),
                rate_limited: true,
            },
            Err(error) => return Err(error),
        };

        // 被交易所以操作次数超限拒绝：继续自动重试只会一直被拒，暂停交给用户处理（第 10 节，
        // B30）。暂停的撤单还会继续发：交易所给撤单的额度比别的操作高。
        if executed.rate_limited {
            self.store
                .save_pause_reason(strategy_id, RATE_LIMITED_NOTE)?;
            self.notify_strategy(
                strategy_id,
                "paused",
                "已暂停",
                RATE_LIMITED_NOTE.to_string(),
            );
            let (status, round) = self
                .pause_inner(strategy_id, &strategy, &mut state, &mut orders, now)
                .await?;
            let report = StepReport {
                strategy_id: strategy_id.clone(),
                status,
                plan,
                placed: executed.placed,
                canceled: withdrawn
                    .into_iter()
                    .chain(executed.canceled)
                    .chain(round.canceled)
                    .collect(),
                applied_fills: snapshot.applied_fills + round.applied_fills,
                position: snapshot.reading.actual,
                expected_position: snapshot.reading.expected,
            };
            let note = join_notes([
                Some(RATE_LIMITED_NOTE.to_string()),
                cancel_only_note(&orders, status, None),
            ]);
            self.save_view(
                strategy_id,
                &report,
                snapshot.available_collateral,
                margin_ratio,
                note,
                now,
            )
            .await?;
            return Ok(report);
        }

        // 格子状态在每次记账时就已经和订单记录一起落盘了。
        self.store.save_fills_synced_through(strategy_id, now)?;
        tracing::info!(
            strategy = %strategy_id,
            market_price = %plan.market_price,
            placed = executed.placed.len(),
            canceled = executed.canceled.len(),
            applied_fills = snapshot.applied_fills,
            position = %snapshot.reading.actual,
            "step done"
        );

        let report = StepReport {
            strategy_id: strategy_id.clone(),
            status: StrategyStatus::Running,
            plan,
            placed: executed.placed,
            canceled: withdrawn.into_iter().chain(executed.canceled).collect(),
            applied_fills: snapshot.applied_fills,
            position: snapshot.reading.actual,
            expected_position: snapshot.reading.expected,
        };
        let note = join_notes([
            unknown.note(),
            margin_alert_note(margin_ratio, &settings),
            self.actions_today().ok().and_then(action_alert_note),
            self.churn_note(strategy_id),
            self.backstop_note(strategy_id),
        ]);
        self.save_view(
            strategy_id,
            &report,
            snapshot.available_collateral,
            margin_ratio,
            note,
            now,
        )
        .await?;
        Ok(report)
    }

    /// 算账户保证金率要看的标的：所有活动策略的标的，加上 `extra`（预览中的草稿）。
    /// 第三方市场的占用按这些标的所在的市场查（第 7.4 节）。
    pub(super) fn margin_instruments(
        &self,
        extra: Option<&Instrument>,
    ) -> Result<Vec<Instrument>, RuntimeError> {
        let mut instruments: Vec<Instrument> = extra.into_iter().cloned().collect();
        for strategy_id in self.store.load_all_strategy_ids()? {
            let stored = self.store.load_strategy(&strategy_id)?;
            if stored.status.is_live() && !instruments.contains(&stored.instrument) {
                instruments.push(stored.instrument);
            }
        }
        Ok(instruments)
    }

    /// 读账户保证金率（第 7.2、7.4 节），连同交易所给出这份读数的时刻。读不到，或者读数
    /// 比 `max_age` 还旧，都是 `None`：闸门按「读不到」关上，不发新单，已在场的挂单保留。
    /// 读不到不让这一步失败。
    pub(super) async fn margin_ratio(
        &self,
        strategy_id: &StrategyId,
        max_age: Duration,
    ) -> Option<(Rate, Option<DateTime<Utc>>)> {
        let instruments = match self.margin_instruments(None) {
            Ok(instruments) => instruments,
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "cannot list the instruments in use");
                return None;
            }
        };
        let margin = match self.account.margin(&instruments).await {
            Ok(margin) => margin,
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "the margin ratio is unavailable");
                return None;
            }
        };
        if let Some(as_of) = margin.as_of
            && !fresh(as_of, self.clock.now(), max_age)
        {
            tracing::warn!(
                strategy = %strategy_id,
                %as_of,
                "the margin reading is stale or from the future"
            );
            return None;
        }
        self.observe_margin(margin.ratio());
        Some((margin.ratio(), margin.as_of))
    }

    /// 保证金率换档时提醒：升到提醒阈值、停止阈值，以及从高档回落到正常。同一档里
    /// 来回读到不同的数不提醒。
    fn observe_margin(&self, ratio: Rate) {
        let settings = match self.store.load_account_settings() {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(%error, "cannot load the account settings to grade the margin ratio");
                return;
            }
        };
        let level = if ratio >= settings.margin_stop_ratio {
            MarginLevel::AboveStop
        } else if ratio >= settings.margin_alert_ratio {
            MarginLevel::AboveAlert
        } else {
            MarginLevel::Normal
        };
        let previous = self
            .margin_level
            .lock()
            .expect("margin level lock poisoned")
            .replace(level);
        let title = match (previous, level) {
            (previous, level) if previous == Some(level) => return,
            // 刚启动、第一次读到正常：没有什么可说的。
            (None, MarginLevel::Normal) => return,
            (_, MarginLevel::AboveStop) => format!(
                "保证金率 {} 达到停止阈值 {}",
                percent(ratio),
                percent(settings.margin_stop_ratio)
            ),
            (_, MarginLevel::AboveAlert) => format!(
                "保证金率 {} 超过提醒阈值 {}",
                percent(ratio),
                percent(settings.margin_alert_ratio)
            ),
            (_, MarginLevel::Normal) => format!("保证金率 {} 已回落", percent(ratio)),
        };
        let body = match level {
            MarginLevel::AboveStop => "全账户只保留只减仓单，回落后自动恢复。",
            MarginLevel::AboveAlert => "",
            MarginLevel::Normal => "低于提醒阈值，策略照常运行。",
        };
        self.notify(Notice::new("account:margin", title, body));
    }

    /// 保证金读数是这一步最先读的，补拉成交、逐张核对订单、几轮仓位核对都排在它后面：链路
    /// 慢的时候走到发单前它已经比行情过期时间还旧，而标记价是刚读的。读数旧了不等于风险
    /// 变了：这里再读一次，闸门状态和判定时一样就按新读数的时刻发单；状态变了就沿用旧时刻，
    /// 这一步不发新单，下一步按新状态重新判定。否则在慢链路上每一步都会因为「行情过期」
    /// 一张单都发不出去，而行情其实是新的（第 7.2 节）。
    ///
    /// 读数还新鲜、或者本来就读不到（闸门已经关着）时什么都不做。
    async fn refresh_margin_if_stale(
        &self,
        strategy_id: &StrategyId,
        judged: MarginState,
        read_at: DateTime<Utc>,
        settings: &AccountSettings,
    ) -> DateTime<Utc> {
        if judged == MarginState::Unavailable
            || self.clock.now().signed_duration_since(read_at) <= settings.market_data_max_age
        {
            return read_at;
        }
        let reading_started_at = self.clock.now();
        let again = self
            .margin_ratio(strategy_id, settings.market_data_max_age)
            .await;
        if margin_state(again.map(|(ratio, _)| ratio), settings) != judged {
            tracing::info!(
                strategy = %strategy_id,
                "the margin state changed while this step was reading; no new orders this step"
            );
            return read_at;
        }
        again
            .and_then(|(_, as_of)| as_of)
            .map_or(reading_started_at, |as_of| as_of.min(reading_started_at))
    }

    /// 保证金率超过停止阈值：马上按库里的 `cloid` 撤掉在场的非只减仓单（第 7.4 节）。
    ///
    /// 放在这一步的其他读取之前：撤单不需要先问到订单状态，后面哪一项读取失败都不能让
    /// 增仓单留在场上。撤不掉的单不报错，下一步规划里还会再撤；写库失败才报错——撤销已经
    /// 发出去了，下一步对账会从交易所读到它的结局。
    async fn withdraw_increasing_now(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        orders: &mut [StoredOrder],
        now: DateTime<Utc>,
    ) -> Result<(Vec<ClientOrderId>, bool), RuntimeError> {
        let instrument = strategy.spec().instrument().clone();
        let mut canceled = Vec::new();
        let mut rate_limited = false;
        for order in orders
            .iter_mut()
            .filter(|order| order.record.is_working() && !order.record.reduce_only)
        {
            match self.cancel_order(&instrument, order, now).await {
                Ok(true) => {
                    order.updated_at = now;
                    self.store.update_order(strategy_id, order)?;
                    canceled.push(order.record.client_order_id.clone());
                }
                Ok(false) => {}
                // 超限：其余的单照样试，撤完再报给调用方暂停（B30）。
                Err(RuntimeError::Exchange(ExchangeError::RateLimited { .. })) => {
                    rate_limited = true;
                }
                Err(error) => tracing::warn!(
                    strategy = %strategy_id,
                    client_order_id = %order.record.client_order_id,
                    %error,
                    "could not withdraw an increasing order, retrying in the plan"
                ),
            }
        }
        if !canceled.is_empty() {
            tracing::warn!(
                strategy = %strategy_id,
                canceled = canceled.len(),
                "margin ratio above the stop threshold, withdrew the increasing orders"
            );
        }
        Ok((canceled, rate_limited))
    }

    /// 判定要暂停时收尾：先落「暂停中」，撤单、对账，返回这一步的报告。
    ///
    /// 暂停是「继续自动重试没有意义」：撤销全部挂单交给用户处理（第 9.2 节）。只是不能
    /// 下新单的情况（仓位数据还在变、有订单结果未知）不走这里，见 [`judge`]。
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn pause_for(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
        snapshot: &Snapshot,
        reason: PauseReason,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        // 日志按真正的暂停原因分：仓位对不上和成交记录对不上要查的东西不一样。
        match reason {
            PauseReason::FillRecordsInconsistent => {
                // 成交流水与订单累计成交量对不上：这不是「数据还在变」，多出来的
                // 流水永远对不平，订单会一直占着 pending、这一格再也挂不出新单。
                // 继续自动重试没有意义，暂停交给用户处理。
                let client_order_id = snapshot
                    .inconsistent
                    .as_ref()
                    .map_or("", ClientOrderId::as_str);
                tracing::error!(
                    strategy = %strategy_id,
                    client_order_id = %client_order_id,
                    "fill records do not add up to the order's cumulative filled quantity, pausing the strategy"
                );
            }
            PauseReason::PositionCheckFailed => {
                // 连续两轮读到同一份数据，仍然解释不了差额：仓位真的对不上。
                tracing::warn!(
                    strategy = %strategy_id,
                    actual = %snapshot.reading.actual,
                    expected = %snapshot.reading.expected,
                    "position check failed, pausing the strategy"
                );
            }
        }
        // 暂停原因单独存：撤单收尾的进度说明会覆盖视图里的说明，暂停之后页面也不再显示
        // 等待原因，不存下来用户就不知道为什么停了。恢复时清掉。
        self.store
            .save_pause_reason(strategy_id, pause_reason_note(reason))?;
        self.notify_strategy(
            strategy_id,
            "paused",
            "已暂停",
            pause_reason_note(reason).to_string(),
        );
        let (status, round) = self
            .pause_inner(strategy_id, strategy, state, orders, now)
            .await?;
        let mut report = snapshot.stop(strategy_id, status, reason.wait_reason(), round.canceled);
        report.applied_fills += round.applied_fills;
        Ok(report)
    }

    /// 读取阶段：向交易所取数，把这一步需要的快照读回来。
    ///
    /// 顺序不能变：
    /// 1. 补记成交——放在核对挂单之前，这段时间里成交的订单，成交记录已经可见，
    ///    不会被误判成撤销；
    /// 2. 收尾不在挂单列表里的订单（[`Runtime::reconcile_orders`]）；
    /// 3. 按主仓成交推进批次、按限速放行下一个单位（第 4.1、4.2 节）——放在仓位核对
    ///    之前，核对用的是状态推算的应有持仓，先推进不会改变它；
    /// 4. 核对仓位（第 7.1.3 节）。
    ///
    /// 差额能用「在场订单已经成交、我们还没记入」解释时不算失败：成交记录可能还没到，
    /// 等到手后状态自然会对上。数据本身也可能还在变——查成交、查挂单状态、查仓位是
    /// 三次独立的读取，中间随时可能又有成交——所以每轮都刷新这三样再比一次，最多比
    /// [`POSITION_CHECK_ROUNDS`] 轮；结论由 [`position_check`] 给出。
    ///
    /// 订单快照必须跟着刷新：`reported` 是上一次核对时的累计量，成交记录还可能查不到，
    /// 沿用旧快照会把「刚成交、还没入账」算成仓位对不上，误暂停一个正常运行的策略。
    /// 稳定性也不能只看净仓位：同一轮里一买一卖相抵时，净仓位和预计仓位都可能一模一样，
    /// 但订单快照已经变了，数据显然还在动。
    ///
    /// 标记价一拿到就看失效价：收到时还新鲜、越过了失效价，就不再做后面的读取，直接报
    /// [`Read::Crossed`]。后面的读取可能失败、也可能拖到行情过期，都不能抹掉已经看到的
    /// 越界（第 7.5 节）。
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn read_step(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        max_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<Read, RuntimeError> {
        let synced = self
            .sync_step(strategy_id, stored, strategy, state, orders, now)
            .await?;
        let priced_at = self.clock.now();
        let market_price = self
            .exchange
            .mark_price(strategy.spec().instrument())
            .await?;
        if fresh(priced_at, self.clock.now(), max_age)
            && strategy.invalidation_triggered(market_price).is_some()
        {
            return Ok(Read::Crossed(market_price));
        }
        let reading = self
            .reading_at(
                strategy_id,
                strategy,
                state,
                orders,
                &synced,
                (market_price, priced_at),
                now,
            )
            .await?;
        Ok(Read::Snapshot(Snapshot {
            market_price: reading.market_price,
            priced_at: reading.priced_at,
            available_collateral: reading.available_collateral,
            reading: reading.reading,
            check: reading.check,
            inconsistent: synced.inconsistent.clone().or(reading.inconsistent),
            applied_fills: synced.applied_fills + reading.applied_fills,
        }))
    }

    /// 订单相关的读取：补记成交、核对挂单、推进批次与限速（[`Runtime::read_step`] 的前半段）。
    ///
    /// 判定和撤单都靠这三样算出的在场订单：撤单要么撤错、要么漏撤，所以它们失败时这一步
    /// 不能继续。行情、可用资金、仓位是另一回事，见 [`Runtime::reading_step`]。
    pub(super) async fn sync_step(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        now: DateTime<Utc>,
    ) -> Result<Synced, RuntimeError> {
        let since = self.fills_since(stored, orders);
        let mut applied_fills = self
            .sync_fills(strategy_id, strategy, state, orders, since, now)
            .await?;

        let reconciled = self
            .reconcile_orders(strategy_id, strategy, state, orders, now)
            .await?;
        applied_fills += reconciled.applied_fills;

        self.advance_progress(strategy_id, strategy, state, orders, now)?;

        Ok(Synced {
            since,
            reported: reconciled.reported,
            inconsistent: reconciled.inconsistent,
            applied_fills,
        })
    }

    /// 展示用的读数：标记价、可用资金、仓位核对（[`Runtime::read_step`] 的后半段）。
    ///
    /// 这些读数只进视图快照和仓位核对，不影响订单本身：拿不到时调用方保留上一份快照
    /// 并说明原因，而不是把已经做成的一步报成失败。
    pub(super) async fn reading_step(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        synced: &Synced,
        now: DateTime<Utc>,
    ) -> Result<Reading, RuntimeError> {
        // 标记价：只进 `Plan` 供展示和日志使用，提前读出来是为了让仓位核对的提前返回
        // 也能报出当时的标记价，而不是一个零值。
        // 行情的年龄从发出请求那一刻算起：交易所的标记价没有时间戳，响应在路上耽搁多久，
        // 都只能算在它的头上，不能拿收到的时刻当成它的时刻。
        let priced_at = self.clock.now();
        let market_price = self
            .exchange
            .mark_price(strategy.spec().instrument())
            .await?;
        self.reading_at(
            strategy_id,
            strategy,
            state,
            orders,
            synced,
            (market_price, priced_at),
            now,
        )
        .await
    }

    /// [`Runtime::reading_step`] 读完标记价之后的部分：可用资金和仓位核对。
    #[allow(clippy::too_many_arguments)]
    async fn reading_at(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        synced: &Synced,
        (market_price, priced_at): (Price, DateTime<Utc>),
        now: DateTime<Utc>,
    ) -> Result<Reading, RuntimeError> {
        let instrument = strategy.spec().instrument();
        let available_collateral = self.account.available_collateral().await?;

        let since = synced.since;
        let mut reported = synced.reported.clone();
        let mut applied_fills = 0;
        let mut inconsistent = None;
        let mut position = self.exchange.position(instrument).await?;
        let mut expected = expected_position(strategy, state, &reported, orders);
        let mut previous: Option<PositionReading> = None;
        for _ in 0..POSITION_CHECK_ROUNDS {
            let reading = PositionReading {
                actual: position,
                expected,
                reported: reported.clone(),
            };
            if position_check(&reading, previous.as_ref()) != PositionCheck::Unstable {
                break;
            }
            previous = Some(reading);
            applied_fills += self
                .sync_fills(strategy_id, strategy, state, orders, since, now)
                .await?;
            let again = self
                .reconcile_orders(strategy_id, strategy, state, orders, now)
                .await?;
            applied_fills += again.applied_fills;
            reported = again.reported;
            inconsistent = inconsistent.or(again.inconsistent);
            self.advance_progress(strategy_id, strategy, state, orders, now)?;
            position = self.exchange.position(instrument).await?;
            expected = expected_position(strategy, state, &reported, orders);
        }

        let reading = PositionReading {
            actual: position,
            expected,
            reported,
        };
        // 循环里每一轮的结论都用来决定「还要不要再看一轮」，最后一份数据的结论就是这
        // 一步的结论：交给判定直接用，判定不必再拼一遍输入。
        let check = position_check(&reading, previous.as_ref());
        Ok(Reading {
            market_price,
            priced_at,
            available_collateral,
            reading,
            check,
            applied_fills,
            inconsistent,
        })
    }

    /// 规划：把判定给出的「被阻塞单位」交给策略，算出这一步要下的单和要撤的单。
    pub(super) fn plan_step(
        &self,
        strategy: &Strategy,
        state: &StrategyState,
        orders: &[StoredOrder],
        blocked: &[PendingUnit],
        market_price: Price,
    ) -> Plan {
        let working_views: Vec<WorkingOrder> = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.record.working_view())
            .collect();
        strategy.plan(state, &working_views, blocked, market_price)
    }

    /// 执行阶段：撤单、下单、记账。
    ///
    /// 撤单始终执行；新单只发判定允许的那几种（`permits`，第 7.2 节）。规划出来的单都留在
    /// `plan.place` 里，页面上看得到闸门打开之后会发什么。
    ///
    /// 撤不掉的订单保留预留；有撤单结果不确定时这一步不下任何新单，避免同一侧出现两张
    /// 都会成交的订单。
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_plan(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
        plan: &mut Plan,
        new_orders: NewOrders,
        now: DateTime<Utc>,
    ) -> Result<Executed, RuntimeError> {
        let cancels = self
            .cancel_planned(strategy_id, strategy.spec().instrument(), orders, plan, now)
            .await?;
        drop_blocked_places(plan, orders);

        let mut rate_limited = false;
        let placed = if cancels.uncertain {
            // 同一步里既有可能过期的未知订单、又有撤销结果不明时，这条原因只报一次。
            if !plan.waiting.contains(&WaitReason::OrderResultUnknown) {
                plan.waiting.push(WaitReason::OrderResultUnknown);
            }
            Vec::new()
        } else {
            // 只减仓单排在前面先发：它们不占保证金，不预检、也不受保证金不足退避的影响（B20），
            // 不该被资金锁和额度查询拖住。同一类里保持规划的顺序。
            let mut intents: Vec<OrderIntent> = plan
                .place
                .iter()
                .filter(|intent| new_orders.permits.allows(intent.reduce_only))
                .cloned()
                .collect();
            intents.sort_by_key(|intent| !intent.reduce_only);
            // 会增加敞口的单要做资金预检（第 10 节）：轮到第一张不在退避里的增仓单时才拿锁、
            // 读一次额度，锁一直持有到这一步发完。读不到额度就做不了预检：增仓单这一步不发。
            let mut _funds_guard = None;
            let mut funds: Option<FundsCheck> = None;
            let mut funds_read = false;
            let mut placed = Vec::new();
            let mut stopped = None;
            let mut short_of_funds = false;
            for intent in &intents {
                // 行情是这一步开头读的：核对、发单花掉的时间里它会变旧。过期之后不再发新单，
                // 剩下的下一步按新行情重新规划（第 7.2 节）。
                if self.market_is_stale(strategy_id, &new_orders) {
                    stopped = Some(WaitReason::StaleMarketData);
                    break;
                }
                let reserved = if intent.reduce_only {
                    None
                } else {
                    if self.in_backoff(strategy_id, intent.unit, intent.purpose, self.clock.now()) {
                        short_of_funds = true;
                        continue;
                    }
                    if !funds_read {
                        funds_read = true;
                        _funds_guard = Some(self.lock_funds().await);
                        funds = match self.funds_check(strategy).await {
                            Ok(check) => Some(check),
                            Err(error) => {
                                tracing::warn!(strategy = %strategy_id, %error, "funds pre-check unavailable");
                                None
                            }
                        };
                    }
                    match funds
                        .as_mut()
                        .and_then(|funds| funds.reserve(intent.quantity, intent.limit_price))
                    {
                        Some(required) => Some(required),
                        None => {
                            short_of_funds = true;
                            continue;
                        }
                    }
                };
                let placement = self
                    .place_one(strategy_id, strategy, state, orders, intent, &new_orders)
                    .await?;
                // 没发出去、或者被拒绝的单不占额度：还回去，后面的单还能用。
                let give_back = |funds: &mut Option<FundsCheck>| {
                    if let (Some(funds), Some(required)) = (funds.as_mut(), reserved) {
                        funds.release(required);
                    }
                };
                let client_order_id = match placement {
                    Placement::Sent(client_order_id) => {
                        if !intent.reduce_only {
                            self.clear_backoff(strategy_id, intent.unit, intent.purpose);
                        }
                        client_order_id
                    }
                    Placement::MarginRejected(client_order_id) => {
                        give_back(&mut funds);
                        // 从收到拒绝的这一刻起算：这一步前面花掉的时间不能算进退避里。
                        self.back_off(strategy_id, intent.unit, intent.purpose, self.clock.now());
                        short_of_funds = true;
                        placed.push(client_order_id);
                        continue;
                    }
                    // 明确拒绝：单没有落地，不占额度，后面的单还能用。
                    Placement::Rejected(client_order_id) => {
                        give_back(&mut funds);
                        placed.push(client_order_id);
                        continue;
                    }
                    // 没发出去的单不进报告：报告说的是这一步做过的动作。
                    Placement::NotSent => {
                        give_back(&mut funds);
                        continue;
                    }
                    Placement::StaleMarket => {
                        give_back(&mut funds);
                        stopped = Some(WaitReason::StaleMarketData);
                        break;
                    }
                    // 操作次数超限：后面的单发了也一样被拒，这一步到此为止，调用方暂停策略。
                    Placement::RateLimited(client_order_id) => {
                        give_back(&mut funds);
                        placed.push(client_order_id);
                        rate_limited = true;
                        break;
                    }
                };
                placed.push(client_order_id.clone());
                // 这张单结果未知：有结论之前本策略不发任何新订单（第 7.2 节），这一步剩下的
                // 单也不发。
                if orders.iter().any(|order| {
                    order.record.client_order_id == client_order_id
                        && order.record.status == OrderStatus::Unknown
                }) {
                    stopped = Some(WaitReason::OrderResultUnknown);
                    break;
                }
            }
            if short_of_funds && !plan.waiting.contains(&WaitReason::InsufficientFunds) {
                plan.waiting.push(WaitReason::InsufficientFunds);
            }
            if let Some(reason) = stopped
                && !plan.waiting.contains(&reason)
            {
                plan.waiting.push(reason);
            }
            placed
        };
        Ok(Executed {
            placed,
            canceled: cancels.canceled,
            rate_limited,
        })
    }

    /// 撤销规划里要撤的订单，逐张落库。
    ///
    /// `Ok(false)` 的订单（结果不确定）保留预留，不计入撤销结果，只记一个「不确定」。
    pub(super) async fn cancel_planned(
        &self,
        strategy_id: &StrategyId,
        instrument: &Instrument,
        orders: &mut [StoredOrder],
        plan: &Plan,
        now: DateTime<Utc>,
    ) -> Result<Cancels, RuntimeError> {
        let mut canceled = Vec::new();
        let mut uncertain = false;
        for client_order_id in &plan.cancel {
            let Some(index) = orders
                .iter()
                .position(|order| &order.record.client_order_id == client_order_id)
            else {
                continue;
            };
            match self
                .cancel_order(instrument, &mut orders[index], now)
                .await?
            {
                true => {
                    orders[index].updated_at = now;
                    self.store.update_order(strategy_id, &orders[index])?;
                    canceled.push(client_order_id.clone());
                    let record = &orders[index].record;
                    if record.cumulative_filled.is_zero() && !record.purpose.is_reduction() {
                        let side = record.unit.side(record.purpose);
                        self.record_churn(strategy_id, side, record.limit_price);
                    }
                }
                false => {
                    uncertain = true;
                    tracing::warn!(
                        strategy = %strategy_id,
                        client_order_id = %client_order_id,
                        "cancel result unknown, keeping the reservation"
                    );
                }
            }
        }
        Ok(Cancels {
            canceled,
            uncertain,
        })
    }

    /// 下一张单：先写数据库，再发送（第 9.1 节）。
    ///
    /// 被拒绝和结果未知的订单也照常报出（[`Placement::Sent`]）：它们都已经落库，是这一步
    /// 尝试过的动作。写完库发现有效期已经不够、或者行情已经过期时，这张单不发，记录也删掉，
    /// 回到「还没发送」之前的样子。
    pub(super) async fn place_one(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
        intent: &OrderIntent,
        new_orders: &NewOrders,
    ) -> Result<Placement, RuntimeError> {
        let mut order = new_order(intent, self.clock.now(), self.options.order_expires_after);
        let client_order_id = order.record.client_order_id.clone();
        // 先写库、后发送（第 9.1 节）：订单在发出去之前必须已经落盘，
        // 否则崩溃后这张单既不在本地记录里、也追不回来。
        self.store.insert_order(strategy_id, &order)?;
        debug_assert!(
            self.store.has_order(&client_order_id)?,
            "the order must be stored before it is sent"
        );
        // 写库花掉的时间里订单会白白少活一段：发送之前按当时的时钟重算到期时刻，
        // 并先落库再发送。
        if let Some(expires_at) = refreshed_expiry(
            order.expires_at,
            self.clock.now(),
            self.options.order_expires_after,
        ) {
            order.expires_at = expires_at;
            self.store.update_order(strategy_id, &order)?;
            // 重算并落库本身也花时间：写完再查一次，有效期还是不够就不发这张单。
            // 发出去只会带着一个已经过去的到期时刻被交易所拒掉，而这一步已经没有时间
            // 再重写一次；删掉记录，让它回到「还没发送」之前的样子，下一步重新规划。
            if self.clock.now() >= order.expires_at {
                self.store.delete_order(strategy_id, &client_order_id)?;
                tracing::warn!(
                    strategy = %strategy_id,
                    client_order_id = %client_order_id,
                    expires_after = ?self.options.order_expires_after,
                    "order not sent: the refreshed expiry is already used up"
                );
                return Ok(Placement::NotSent);
            }
        }
        // 写库同样花时间：发送前最后看一眼行情，过期了就不发。
        if self.market_is_stale(strategy_id, new_orders) {
            self.store.delete_order(strategy_id, &client_order_id)?;
            return Ok(Placement::StaleMarket);
        }

        let outcome = self
            .send_order(strategy_id, strategy, intent, &mut order)
            .await?;
        match outcome {
            // 下单即成交：当场记账，避免这一步结束时仓位对不上。
            // 格子状态和订单记录写在同一个事务里。
            //
            // 成交时刻取收到回执的时刻：交易所的回执没有时间戳，这是唯一已知的时刻。
            // 成交发生在发送之后、收到回执之前，所以它一定不早于真实成交时刻，是保守的
            // 那个上界；真实流水到了会把它改回实际成交时刻（第 4.1 节）。
            SendOutcome::Accepted(Some(filled)) => {
                strategy.apply_fill(state, &mut order.record, filled, self.clock.now());
                self.store
                    .save_applied_fills(strategy_id, state, &[&order])?;
            }
            SendOutcome::Accepted(None)
            | SendOutcome::Rejected
            | SendOutcome::MarginRejected
            | SendOutcome::RateLimited
            | SendOutcome::Unknown => self.store.update_order(strategy_id, &order)?,
        }
        orders.push(order);
        Ok(match outcome {
            SendOutcome::MarginRejected => Placement::MarginRejected(client_order_id),
            SendOutcome::Rejected => Placement::Rejected(client_order_id),
            SendOutcome::RateLimited => Placement::RateLimited(client_order_id),
            SendOutcome::Accepted(_) | SendOutcome::Unknown => Placement::Sent(client_order_id),
        })
    }

    /// 行情是不是已经过期：从读行情的请求发出那一刻算起，超过账户风控设置里的行情过期
    /// 时间就算（第 7.2 节）。
    fn market_is_stale(&self, strategy_id: &StrategyId, new_orders: &NewOrders) -> bool {
        let age = self.clock.now().signed_duration_since(new_orders.priced_at);
        let stale = age > new_orders.max_age;
        if stale {
            tracing::warn!(
                strategy = %strategy_id,
                age_ms = age.num_milliseconds(),
                "the market price is stale, no more new orders this step"
            );
        }
        stale
    }

    /// 把订单发出去，按交易所的回答更新本地记录。
    ///
    /// 返回交易所当场报出的累计成交量（下单即成交的数量）；没有成交返回 `None`。
    pub(super) async fn send_order(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        intent: &OrderIntent,
        order: &mut StoredOrder,
    ) -> Result<SendOutcome, RuntimeError> {
        let request = OrderRequest {
            instrument: strategy.spec().instrument().clone(),
            client_order_id: order.record.client_order_id.clone(),
            side: intent.side,
            limit_price: intent.limit_price,
            quantity: intent.quantity,
            reduce_only: intent.reduce_only,
            // 失效减仓和市价平仓按标记价让出滑点上限去成交，没成交的部分不能留在场上：下一步
            // 按新的标记价重新算（B38、B51）。
            time_in_force: if intent.purpose.is_reduction() {
                TimeInForce::ImmediateOrCancel
            } else {
                TimeInForce::GoodTillCanceled
            },
            expires_at: order.expires_at,
        };
        self.count_action();
        match self.exchange.place_order(&request).await {
            Ok(receipt) => {
                order.exchange_order_id = Some(receipt.exchange_order_id);
                order.record.status = receipt.status;
                if receipt.cumulative_filled.is_zero() {
                    return Ok(SendOutcome::Accepted(None));
                }
                Ok(SendOutcome::Accepted(Some(receipt.cumulative_filled)))
            }
            Err(ExchangeError::Rejected { reason }) => {
                order.record.status = OrderStatus::Rejected;
                // 被拒绝的订单没有落到交易所，没有成交要核对。
                order.fills_reconciled = true;
                tracing::warn!(
                    strategy = %strategy_id,
                    client_order_id = %order.record.client_order_id,
                    reason = %reason,
                    "order rejected"
                );
                Ok(SendOutcome::Rejected)
            }
            // 保证金不足同样是明确拒绝：按拒绝记，调用方据此退避（第 10 节）。
            Err(ExchangeError::InsufficientMargin { reason }) => {
                order.record.status = OrderStatus::Rejected;
                order.fills_reconciled = true;
                tracing::warn!(
                    strategy = %strategy_id,
                    client_order_id = %order.record.client_order_id,
                    reason = %reason,
                    "order rejected for insufficient margin, backing off"
                );
                Ok(SendOutcome::MarginRejected)
            }
            // 操作次数超限同样是明确拒绝：单没有落地（第 10 节，B30）。
            Err(ExchangeError::RateLimited { reason }) => {
                order.record.status = OrderStatus::Rejected;
                order.fills_reconciled = true;
                tracing::error!(
                    strategy = %strategy_id,
                    client_order_id = %order.record.client_order_id,
                    reason = %reason,
                    "order rejected for too many requests, pausing the strategy"
                );
                Ok(SendOutcome::RateLimited)
            }
            Err(error) => {
                order.record.status = OrderStatus::Unknown;
                tracing::warn!(
                    strategy = %strategy_id,
                    client_order_id = %order.record.client_order_id,
                    error = %error,
                    "order result unknown"
                );
                Ok(SendOutcome::Unknown)
            }
        }
    }

    /// 按主仓成交推进批次（第 4.2 节）、按限速放行下一个单位（第 4.1 节），有变化就落盘。
    ///
    /// 记账之后、规划之前调用：新启用的格子和新放行的单位这一步就会挂上单，
    /// 提前结束的批次这一步就不再补挂主仓买单。
    ///
    /// 放行时刻由时钟给出，判断依据是记下来的真实成交时刻：重启和暂停都不会多给额度
    /// （第 9.4 节）。
    pub(super) fn advance_progress(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &[StoredOrder],
        now: DateTime<Utc>,
    ) -> Result<(), RuntimeError> {
        let working: Vec<WorkingOrder> = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.record.working_view())
            .collect();
        let advanced = strategy.advance(state, &working);
        let released = strategy.advance_throttle(state, now);
        if advanced || released {
            // 只改了状态，没有新的成交要记，订单记录不用动。
            self.store.save_applied_fills(strategy_id, state, &[])?;
        }
        Ok(())
    }
}

/// 账户保证金率换成闸门的输入（第 7.2、7.4 节）：读不到就是「读不到」，达到停止阈值就是
/// 「超过停止阈值」。
fn margin_state(ratio: Option<Rate>, settings: &AccountSettings) -> MarginState {
    match ratio {
        None => MarginState::Unavailable,
        Some(ratio) if ratio >= settings.margin_stop_ratio => MarginState::AboveStop,
        Some(_) => MarginState::Normal,
    }
}

/// 保证金率达到提醒阈值时的提醒（第 7.4 节）。达到停止阈值时说明只减仓的处理。
fn margin_alert_note(ratio: Option<Rate>, settings: &AccountSettings) -> Option<String> {
    let ratio = ratio?;
    if ratio >= settings.margin_stop_ratio {
        Some(format!(
            "保证金率 {} 达到停止阈值 {}：全账户只保留只减仓单，回落后自动恢复。",
            percent(ratio),
            percent(settings.margin_stop_ratio)
        ))
    } else if ratio >= settings.margin_alert_ratio {
        Some(format!(
            "保证金率 {} 超过提醒阈值 {}。",
            percent(ratio),
            percent(settings.margin_alert_ratio)
        ))
    } else {
        None
    }
}

/// 判定要求暂停时写给用户看的原因（第 7.1 节，B18）。
fn pause_reason_note(reason: PauseReason) -> &'static str {
    match reason {
        PauseReason::PositionCheckFailed => {
            "仓位和应有持仓对不上，已暂停：核对交易所仓位（是否手动交易过）后再恢复。"
        }
        PauseReason::FillRecordsInconsistent => {
            "成交记录和订单累计成交量对不上，已暂停：核对该单成交后再恢复。"
        }
    }
}

/// 被交易所以操作次数超限拒绝之后暂停的原因（B30）。
const RATE_LIMITED_NOTE: &str =
    "操作次数超限被交易所拒绝，已暂停：减少格子或加大间距 d，额度恢复后再恢复。";

/// 当日操作次数超过上限的 80% 时的提醒（第 10 节，B30）。
fn action_alert_note(actions: u64) -> Option<String> {
    (actions * 10 > DAILY_ACTION_LIMIT * 8).then(|| {
        format!(
            "今日操作次数 {actions}，超过上限 {DAILY_ACTION_LIMIT} 的 80%：考虑减少格子或加大间距 d。"
        )
    })
}

/// 把几句说明接成一段；一句都没有时是 `None`。
fn join_notes<const N: usize>(notes: [Option<String>; N]) -> Option<String> {
    let notes: Vec<String> = notes.into_iter().flatten().collect();
    (!notes.is_empty()).then(|| notes.concat())
}

/// 保证金率超过停止阈值：在场的非只减仓单全部撤掉，账户只保留只减仓单（第 7.4 节）。
///
/// 单侧网格的格子平仓单本来就是只减仓单，照常留着；主仓买单、格子建仓单和中性网格的
/// 格子单（可能让净仓位穿越零）都撤。规划里本来就要撤的单不重复撤。
fn withdraw_increasing(plan: &mut Plan, orders: &[StoredOrder]) {
    for order in orders
        .iter()
        .filter(|order| order.record.is_working() && !order.record.reduce_only)
    {
        if !plan.cancel.contains(&order.record.client_order_id) {
            plan.cancel.push(order.record.client_order_id.clone());
        }
    }
}

/// 运行中一步的读取结果。
pub(super) enum Read {
    /// 判定需要的全部读数。
    Snapshot(Snapshot),
    /// 读到的标记价收到时还新鲜，已经越过了失效价：这一步改走失效。
    Crossed(Price),
}

/// 交易所对一张单的回答。
pub(super) enum SendOutcome {
    /// 收下了；带着当场成交的累计数量（没有成交是 `None`）。
    Accepted(Option<Quantity>),
    /// 明确拒绝。
    Rejected,
    /// 以保证金不足明确拒绝：调用方要退避（第 10 节）。
    MarginRejected,
    /// 以操作次数超限明确拒绝：调用方要暂停策略（B30）。
    RateLimited,
    /// 结果未知。
    Unknown,
}

/// 发一张单的结果。
pub(super) enum Placement {
    /// 发出去了，交易所收下了或者结果未知：记录留在库里。
    Sent(ClientOrderId),
    /// 发出去了，交易所明确拒绝：记录留在库里，单没有落地。
    Rejected(ClientOrderId),
    /// 发出去了，交易所以保证金不足拒绝：记录留在库里，这个单位要退避。
    MarginRejected(ClientOrderId),
    /// 发出去了，交易所以操作次数超限拒绝：记录留在库里，单没有落地，策略要暂停。
    RateLimited(ClientOrderId),
    /// 写完库发现有效期已经不够，没发；记录已删掉。
    NotSent,
    /// 写完库发现行情已经过期，没发；记录已删掉。这一步剩下的单也不该再发。
    StaleMarket,
}

/// 一张还没发出去的订单。
///
/// `expiresAfter` 按这张单实际发出去的时刻算，不是这一步开始的时刻：一步里可能挂好几张
/// 单，套用步首时间会让后面的单少活一段时间（第 9.1 节）。
pub(super) fn new_order(
    intent: &OrderIntent,
    sent_at: DateTime<Utc>,
    expires_after: Duration,
) -> StoredOrder {
    StoredOrder {
        record: OrderRecord {
            client_order_id: new_client_order_id(),
            unit: intent.unit,
            purpose: intent.purpose,
            limit_price: intent.limit_price,
            quantity: intent.quantity,
            cumulative_filled: Quantity::ZERO,
            reduce_only: intent.reduce_only,
            status: OrderStatus::Submitting,
        },
        exchange_order_id: None,
        fills_reconciled: false,
        expires_at: sent_at + expires_after,
        placed_at: sent_at,
        updated_at: sent_at,
    }
}

/// 仓位核对时我们预计交易所会有的仓位。
///
/// 由格子状态算出的应有持仓，加上交易所已经成交、我们还没记入的数量：
/// 成交记录可能比挂单状态晚到，这段差额是能解释的（第 7.1.3 节）。
pub(super) fn expected_position(
    strategy: &Strategy,
    state: &StrategyState,
    reported: &[(ClientOrderId, Quantity)],
    orders: &[StoredOrder],
) -> Position {
    expected_from(
        strategy.expected_position(state),
        reported,
        &order_states(orders),
    )
}

/// 撤单之后重新算一次「哪些单位还没对完账」，把被挡住的订单从规划里去掉。
///
/// 撤销只把订单变成终态：撤销前可能已经有成交、成交记录也可能还没到，最终成交量
/// 还不知道。为同一个单位、同一用途规划出来的替换单必须等它对完账、按最终累计成交量
/// 记账之后再按新状态重算数量，否则会拿撤单前的数量再下一次单——撤单期间成交了一部分
/// 时，两次加起来就超过了这一格该持有的量（第 9.2 节）。
pub(super) fn drop_blocked_places(plan: &mut Plan, orders: &[StoredOrder]) {
    let blocked = pending_units(&order_states(orders));
    let is_blocked = |intent: &OrderIntent| {
        blocked.contains(&PendingUnit {
            unit: intent.unit,
            purpose: intent.purpose,
        })
    };
    if !plan.place.iter().any(is_blocked) {
        return;
    }
    plan.place.retain(|intent| !is_blocked(intent));
    if !plan.waiting.contains(&WaitReason::FillsPending) {
        plan.waiting.push(WaitReason::FillsPending);
    }
}

/// 发送之前要不要重算订单的到期时刻，返回重算后的值；`None` 表示不用改。
///
/// 写库花掉的时间里订单会白白少活一段，所以要按发送前的时钟重算。时钟没往前走的
/// 时候返回 `None`：这时重算出来的值一模一样，再写一次库没有意义（第 9.1 节）。
pub(super) fn refreshed_expiry(
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
    expires_after: Duration,
) -> Option<DateTime<Utc>> {
    let fresh = now + expires_after;
    (fresh > expires_at).then_some(fresh)
}

pub(super) fn new_client_order_id() -> ClientOrderId {
    ClientOrderId::try_new(format!("0x{}", uuid::Uuid::new_v4().simple()))
        .expect("a generated client order id is never empty")
}

#[cfg(test)]
mod tests {
    use super::refreshed_expiry;
    use chrono::{DateTime, Duration, TimeZone, Utc};

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap() + Duration::seconds(seconds)
    }

    /// 时钟没往前走时不重算：重算出来的值一模一样，再写一次库没有意义。
    #[test]
    fn an_expiry_is_only_recomputed_when_the_clock_moved_on() {
        let expires_after = Duration::seconds(5);

        assert_eq!(refreshed_expiry(at(10), at(5), expires_after), None);
        // 时钟往前走了：重算，订单从这一刻开始活 `expiresAfter`。
        assert_eq!(refreshed_expiry(at(10), at(6), expires_after), Some(at(11)));
    }
}
