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

use crate::exchange::{ExchangeError, OrderRequest};
use crate::settings::AccountSettings;
use crate::store::{StoredOrder, StoredStrategy, StrategyId, StrategyStatus};

use super::cancel_only::cancel_only_note;
use super::{
    Cancels, Executed, NewOrders, POSITION_CHECK_ROUNDS, Reading, Runtime, RuntimeError, Snapshot,
    StepReport, Synced, order_states,
};

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
        match stored.status {
            status if status.is_cancel_only() => {
                self.cancel_only_step(strategy_id, &stored, now).await
            }
            StrategyStatus::Restoring => self.restore_step(strategy_id, now).await,
            StrategyStatus::Running => self.running_step(strategy_id, now).await,
            StrategyStatus::Stopped => Err(RuntimeError::Archived(strategy_id.clone())),
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
        let withdrawn = if margin == MarginState::AboveStop {
            self.withdraw_increasing_now(strategy_id, &strategy, &mut orders, now)
                .await?
        } else {
            Vec::new()
        };

        let snapshot = self
            .read_step(
                strategy_id,
                &stored,
                &strategy,
                &mut state,
                &mut orders,
                now,
            )
            .await?;
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
        let executed = self
            .execute_plan(
                strategy_id,
                &strategy,
                &mut state,
                &mut orders,
                &mut plan,
                NewOrders {
                    permits: decision.permits,
                    // 读数里最旧的那个时刻：保证金读数本身可能早于这一步开始（第 7.2 节）。
                    priced_at: margin_reading
                        .and_then(|(_, as_of)| as_of)
                        .map_or(reads_started_at, |as_of| as_of.min(reads_started_at))
                        .min(snapshot.priced_at),
                    max_age: settings.market_data_max_age,
                },
                now,
            )
            .await?;

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
        let note = join_notes([unknown.note(), margin_alert_note(margin_ratio, &settings)]);
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

    /// 读账户保证金率（第 7.2、7.4 节），连同交易所给出这份读数的时刻。读不到，或者读数
    /// 比 `max_age` 还旧，都是 `None`：闸门按「读不到」关上，不发新单，已在场的挂单保留。
    /// 读不到不让这一步失败。
    pub(super) async fn margin_ratio(
        &self,
        strategy_id: &StrategyId,
        max_age: Duration,
    ) -> Option<(Rate, Option<DateTime<Utc>>)> {
        let margin = match self.account.margin().await {
            Ok(margin) => margin,
            Err(error) => {
                tracing::warn!(strategy = %strategy_id, %error, "the margin ratio is unavailable");
                return None;
            }
        };
        if let Some(as_of) = margin.as_of {
            let age = self.clock.now().signed_duration_since(as_of);
            if age > max_age {
                tracing::warn!(
                    strategy = %strategy_id,
                    age_ms = age.num_milliseconds(),
                    "the margin reading is stale"
                );
                return None;
            }
        }
        Some((margin.ratio(), margin.as_of))
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
    ) -> Result<Vec<ClientOrderId>, RuntimeError> {
        let instrument = strategy.spec().instrument().clone();
        let mut canceled = Vec::new();
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
        Ok(canceled)
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
    pub(super) async fn read_step(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        now: DateTime<Utc>,
    ) -> Result<Snapshot, RuntimeError> {
        let synced = self
            .sync_step(strategy_id, stored, strategy, state, orders, now)
            .await?;
        let reading = self
            .reading_step(strategy_id, strategy, state, orders, &synced, now)
            .await?;
        Ok(Snapshot {
            market_price: reading.market_price,
            priced_at: reading.priced_at,
            available_collateral: reading.available_collateral,
            reading: reading.reading,
            check: reading.check,
            inconsistent: synced.inconsistent.clone().or(reading.inconsistent),
            applied_fills: synced.applied_fills + reading.applied_fills,
        })
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
        let instrument = strategy.spec().instrument();
        // 标记价：只进 `Plan` 供展示和日志使用，提前读出来是为了让仓位核对的提前返回
        // 也能报出当时的标记价，而不是一个零值。
        // 行情的年龄从发出请求那一刻算起：交易所的标记价没有时间戳，响应在路上耽搁多久，
        // 都只能算在它的头上，不能拿收到的时刻当成它的时刻。
        let priced_at = self.clock.now();
        let market_price = self.exchange.mark_price(instrument).await?;
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

        let placed = if cancels.uncertain {
            // 同一步里既有可能过期的未知订单、又有撤销结果不明时，这条原因只报一次。
            if !plan.waiting.contains(&WaitReason::OrderResultUnknown) {
                plan.waiting.push(WaitReason::OrderResultUnknown);
            }
            Vec::new()
        } else {
            let intents: Vec<OrderIntent> = plan
                .place
                .iter()
                .filter(|intent| new_orders.permits.allows(intent.reduce_only))
                .cloned()
                .collect();
            let mut placed = Vec::new();
            let mut stopped = None;
            for intent in &intents {
                // 行情是这一步开头读的：核对、发单花掉的时间里它会变旧。过期之后不再发新单，
                // 剩下的下一步按新行情重新规划（第 7.2 节）。
                if self.market_is_stale(strategy_id, &new_orders) {
                    stopped = Some(WaitReason::StaleMarketData);
                    break;
                }
                let client_order_id = match self
                    .place_one(strategy_id, strategy, state, orders, intent, &new_orders)
                    .await?
                {
                    Placement::Sent(client_order_id) => client_order_id,
                    // 没发出去的单不进报告：报告说的是这一步做过的动作。
                    Placement::NotSent => continue,
                    Placement::StaleMarket => {
                        stopped = Some(WaitReason::StaleMarketData);
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

        match self
            .send_order(strategy_id, strategy, intent, &mut order)
            .await?
        {
            // 下单即成交：当场记账，避免这一步结束时仓位对不上。
            // 格子状态和订单记录写在同一个事务里。
            //
            // 成交时刻取收到回执的时刻：交易所的回执没有时间戳，这是唯一已知的时刻。
            // 成交发生在发送之后、收到回执之前，所以它一定不早于真实成交时刻，是保守的
            // 那个上界；真实流水到了会把它改回实际成交时刻（第 4.1 节）。
            Some(filled) => {
                strategy.apply_fill(state, &mut order.record, filled, self.clock.now());
                self.store
                    .save_applied_fills(strategy_id, state, &[&order])?;
            }
            None => self.store.update_order(strategy_id, &order)?,
        }
        orders.push(order);
        Ok(Placement::Sent(client_order_id))
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
    ) -> Result<Option<Quantity>, RuntimeError> {
        let request = OrderRequest {
            instrument: strategy.spec().instrument().clone(),
            client_order_id: order.record.client_order_id.clone(),
            side: intent.side,
            limit_price: intent.limit_price,
            quantity: intent.quantity,
            reduce_only: intent.reduce_only,
            expires_at: order.expires_at,
        };
        match self.exchange.place_order(&request).await {
            Ok(receipt) => {
                order.exchange_order_id = Some(receipt.exchange_order_id);
                order.record.status = receipt.status;
                if receipt.cumulative_filled.is_zero() {
                    return Ok(None);
                }
                Ok(Some(receipt.cumulative_filled))
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
                Ok(None)
            }
            Err(error) => {
                order.record.status = OrderStatus::Unknown;
                tracing::warn!(
                    strategy = %strategy_id,
                    client_order_id = %order.record.client_order_id,
                    error = %error,
                    "order result unknown"
                );
                Ok(None)
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
            "账户保证金率 {} 达到停止阈值 {}：同一账户所有策略只发、只保留只减仓单，回落后自动恢复。",
            percent(ratio),
            percent(settings.margin_stop_ratio)
        ))
    } else if ratio >= settings.margin_alert_ratio {
        Some(format!(
            "账户保证金率 {} 超过提醒阈值 {}。",
            percent(ratio),
            percent(settings.margin_alert_ratio)
        ))
    } else {
        None
    }
}

/// 比率按百分比写，保留两位小数。
fn percent(ratio: Rate) -> String {
    let hundredths = i128::from(ratio.units()) * 10_000 / i128::from(poise_strategy::units::SCALE);
    format!("{}.{:02}%", hundredths / 100, hundredths % 100)
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

/// 发一张单的结果。
pub(super) enum Placement {
    /// 发出去了（含被拒绝和结果未知的）：记录留在库里。
    Sent(ClientOrderId),
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
