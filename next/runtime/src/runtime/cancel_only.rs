//! 只撤单的状态：暂停中、停止中、恢复中，以及进入和离开它们的操作（第 2.4、9.4 节）。

use chrono::{DateTime, Utc};
use poise_strategy::Strategy;
use poise_strategy::order::{ClientOrderId, OrderStatus};
use poise_strategy::planning::Plan;
use poise_strategy::state::StrategyState;
use poise_strategy::units::{Position, Price};

use crate::store::{StoredOrder, StoredStrategy, StrategyId, StrategyStatus};

use super::{CancelOnly, CancelOnlyRound, ResumeOutcome, Runtime, RuntimeError, StepReport};

impl Runtime {
    /// 暂停：先落「暂停中」，再撤单收尾（第 9.4 节，第 2.4 节）。
    ///
    /// 撤不干净（撤销结果不明、成交还没对完账）时不报错：状态停在「暂停中」，常驻服务
    /// 继续撤、继续对账，收尾之后自己转成「已暂停」。报错只会让用户反复点，而每点一次
    /// 都只是重发一遍撤销。
    ///
    /// 恢复中的策略也能暂停：它遗留的订单还没撤掉，暂停正好把它们撤干净。
    pub async fn pause(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Vec<ClientOrderId>, RuntimeError> {
        let _guard = self.begin_waiting(strategy_id).await?;
        let now = self.clock.now();
        self.refuse_archived(strategy_id)?;
        let status = self.store.load_strategy(strategy_id)?.status;
        match status {
            // 已经暂停：再点一次不报错，结果一样。
            StrategyStatus::Paused => return Ok(Vec::new()),
            StrategyStatus::Running | StrategyStatus::Restoring | StrategyStatus::Pausing => {}
            other => return Err(RuntimeError::NotRunning(strategy_id.clone(), other)),
        }
        if status != StrategyStatus::Pausing {
            self.store
                .save_strategy_status(strategy_id, StrategyStatus::Pausing, now)?;
        }
        let report = self.cancel_only_step_by_id(strategy_id, now).await?;
        Ok(report.canceled)
    }

    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    ///
    /// 判定说仓位对不上而暂停时走这里：状态先落「暂停中」，再撤单收尾。返回收尾后的
    /// 状态和这一步撤销的订单。
    pub(super) async fn pause_inner(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
        now: DateTime<Utc>,
    ) -> Result<(StrategyStatus, CancelOnlyRound), RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        // 先落「暂停中」：从这里开始这个策略不再下任何新单，即使这一步之后进程崩了，
        // 重启后驱动的也是只撤单的那条路。
        self.store
            .save_strategy_status(strategy_id, StrategyStatus::Pausing, now)?;
        // 走的是和「暂停中」的驱动、恢复一样的那一轮：撤单只看库里的 `cloid`，
        // 同步失败就丢掉内存、重新加载再撤（[`Runtime::cancel_only_round`]）。
        let round = self
            .cancel_only_round(strategy_id, &stored, strategy, state, orders, now)
            .await?;
        let status = if round.is_clean() && cancel_only_settled(orders) {
            self.store
                .save_strategy_status(strategy_id, StrategyStatus::Paused, now)?;
            tracing::warn!(
                strategy = %strategy_id,
                canceled = round.canceled.len(),
                "strategy paused"
            );
            StrategyStatus::Paused
        } else {
            tracing::warn!(
                strategy = %strategy_id,
                canceled = round.canceled.len(),
                "strategy pausing: some orders are not settled yet"
            );
            StrategyStatus::Pausing
        };
        Ok((status, round))
    }

    /// 停止：先落「停止中」，撤单收尾之后再归档（第 2.4 节，B45）。
    ///
    /// 和暂停的区别是它到此为止：归档之后不能再驱动、不能再改参数、也不能恢复。
    /// 已经停止的策略再停一次不报错——结果一样，网页连点两次不该看到失败。
    ///
    /// 撤不干净的订单不会留在无人看管的地方：状态停在「停止中」，常驻服务继续撤、
    /// 继续对账，全部订单终态且成交对完账之后才归档。
    pub async fn stop(&self, strategy_id: &StrategyId) -> Result<Vec<ClientOrderId>, RuntimeError> {
        let _guard = self.begin_waiting(strategy_id).await?;
        let now = self.clock.now();
        let status = self.store.load_strategy(strategy_id)?.status;
        match status {
            StrategyStatus::Stopped => return Ok(Vec::new()),
            StrategyStatus::Draft => {
                return Err(RuntimeError::NotRunning(strategy_id.clone(), status));
            }
            StrategyStatus::Running
            | StrategyStatus::Restoring
            | StrategyStatus::Pausing
            | StrategyStatus::Paused
            | StrategyStatus::Stopping => {}
        }
        if status != StrategyStatus::Stopping {
            self.store
                .save_strategy_status(strategy_id, StrategyStatus::Stopping, now)?;
        }
        let report = self.cancel_only_step_by_id(strategy_id, now).await?;
        Ok(report.canceled)
    }

    /// 只撤单的一步（第 2.4 节）：不下任何新单，只撤掉在场订单、按 `cloid` 查终态、
    /// 补记成交；全部订单终态且成交对完账之后，转成「已暂停」或「已归档」。
    ///
    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    pub(super) async fn cancel_only_step_by_id(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        self.cancel_only_step(strategy_id, &stored, now).await
    }

    pub(super) async fn cancel_only_step(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        let target = match stored.status {
            StrategyStatus::Pausing => StrategyStatus::Paused,
            StrategyStatus::Stopping => StrategyStatus::Stopped,
            other => return Err(RuntimeError::NotRunning(strategy_id.clone(), other)),
        };
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let mut state = self.load_state(strategy_id, &strategy)?;
        let mut orders = self.store.load_orders(strategy_id)?;

        // 判定不参与：这一步只撤单，判定出来的「该下什么单」在这里没有意义。
        let round = self
            .cancel_only_round(strategy_id, stored, &strategy, &mut state, &mut orders, now)
            .await?;

        // 收场：撤销和核对都走通，而且没有还在场的订单、成交也对完了账。任何一样没做到
        // 都留在「暂停中」「停止中」，由后台继续推（第 9.2 节）。
        let settled = round.is_clean() && cancel_only_settled(&orders);
        let status = if settled {
            self.store.save_strategy_status(strategy_id, target, now)?;
            tracing::warn!(
                strategy = %strategy_id,
                status = target.as_str(),
                canceled = round.canceled.len(),
                "cancel-only step finished"
            );
            target
        } else {
            tracing::warn!(
                strategy = %strategy_id,
                status = stored.status.as_str(),
                working = orders
                    .iter()
                    .filter(|order| order.record.is_working())
                    .count(),
                "cancel-only step is not finished yet"
            );
            stored.status
        };

        // 展示用的读数放在最后，而且失败不算这一步失败：撤销已经发出去、终态也已经核对，
        // 这时候报错只会让用户看到「暂停失败」而单其实已经撤了。拿不到读数就沿用上一份
        // 快照，并在说明里写清楚为什么没更新（第 11 节）。
        //
        // 这一轮没走通就不取展示读数了：仓位核对要拿交易所报的累计成交量当输入，缺了它
        // 算出来的「应有持仓」是错的；而且核对循环里还会再记账、再写库，内存刚从库里
        // 重新加载过，这一轮的结论本来就不算数。报出去还不如沿用上一份。
        let reading = match (&round.synced, &round.cancel_error) {
            (Err(error), _) => Err(format!("成交流水和订单状态（{error}）")),
            (Ok(_), Some(error)) => Err(format!("撤单或撤单后的订单核对（{error}）")),
            (Ok(synced), None) => match self
                .reading_step(strategy_id, &strategy, &mut state, &mut orders, synced, now)
                .await
            {
                Ok(reading) => Ok(reading),
                Err(error) => {
                    tracing::warn!(
                        strategy = %strategy_id,
                        %error,
                        "the display readings are unavailable, keeping the previous view"
                    );
                    Err(format!("行情、可用资金或仓位（{error}）"))
                }
            },
        };

        let (position, expected_position, market_price) = match &reading {
            Ok(reading) => (
                reading.reading.actual,
                reading.reading.expected,
                reading.market_price,
            ),
            Err(_) => {
                let previous = self.store.load_view(strategy_id)?;
                (previous.position, previous.expected_position, Price::ZERO)
            }
        };
        let report = StepReport {
            strategy_id: strategy_id.clone(),
            status,
            plan: Plan {
                market_price,
                ..Plan::default()
            },
            placed: Vec::new(),
            canceled: round.canceled,
            applied_fills: round.applied_fills
                + reading.as_ref().map_or(0, |reading| reading.applied_fills),
            position,
            expected_position,
        };

        match reading {
            Ok(reading) => {
                self.save_view(strategy_id, &report, reading.available_collateral, now)
                    .await?;
            }
            Err(what) => {
                let note = stale_reading_note(&what, cancel_only_note(&orders, status));
                self.annotate_view(strategy_id, status, note).await?;
            }
        }
        Ok(report)
    }

    /// 只撤单的一轮：同步（补记成交、核对订单）→ 出错就丢弃本轮内存、从库里重新加载
    /// → 按持久化下来的 `cloid` 撤单 → 按 `cloid` 核对终态。
    ///
    /// 暂停、停止、恢复共用这一段：三处对「撤哪几张单」的要求是一样的，差别只在收场之后
    /// 进入哪个状态。各写一份的话，同步那一步的失败处理迟早会漏掉一处——恢复那一处就漏过
    /// 一次，结果是订单查询一直失败时遗留挂单一张都不撤（第 9.2 节）。
    ///
    /// 撤单只靠库里存着的 `cloid`，不需要先问到订单状态，所以同步失败也要照撤。
    pub(super) async fn cancel_only_round(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
        now: DateTime<Utc>,
    ) -> Result<CancelOnlyRound, RuntimeError> {
        let mut applied_fills = 0;
        let synced = match self
            .sync_step(strategy_id, stored, strategy, state, orders, now)
            .await
        {
            Ok(synced) => {
                applied_fills += synced.applied_fills;
                Ok(synced)
            }
            Err(sync_error) => {
                tracing::warn!(
                    strategy = %strategy_id,
                    %sync_error,
                    "the order reads failed, canceling the orders anyway"
                );
                // 丢弃这一轮改过的内存，重新加载落盘的那一份。补记成交先写流水、再写格子
                // 和订单的累计成交量，两步不是一个事务：写第二步失败时内存里已经改过了，
                // 继续拿它撤单，订单那一笔会被单独写进库、格子却停在原处——下一轮核对时
                // 累计量已经相等，永远不会补记，这一格就永久对不上了。
                //
                // 重新加载的是库里的那份，撤单接着用它：撤哪几张单只认 `cloid`，不需要
                // 刚才那几次没成功的查询。但这一轮算没走通，调用方不能宣告完成——同步失败
                // 意味着成交没核对完，这一轮报出来的收场结论不算数。
                *state = self.load_state(strategy_id, strategy)?;
                *orders = self.store.load_orders(strategy_id)?;
                Err(sync_error)
            }
        };

        let cancel_only = self
            .cancel_only(strategy_id, strategy, state, orders, now)
            .await;
        applied_fills += cancel_only.applied_fills;
        let cancel_error = cancel_only.cancel_error.or(cancel_only.reconcile_error);
        if cancel_error.is_some() {
            // 撤完之后的核对同样会记账：写库失败时内存里的格子和订单已经改过了，道理和
            // 上面同步失败一样，丢掉内存、重新加载落盘的那一份。撤销本身已经逐张落库，
            // 重新加载不会丢掉它们。
            *state = self.load_state(strategy_id, strategy)?;
            *orders = self.store.load_orders(strategy_id)?;
        }
        Ok(CancelOnlyRound {
            canceled: cancel_only.canceled,
            applied_fills,
            synced,
            cancel_error,
        })
    }

    /// 撤掉全部在场订单，再按 `cloid` 核对一遍终态、补记成交。
    ///
    /// 撤销只说明我们发过撤销请求：撤销前可能已经有成交、成交记录也可能还没到，撤销
    /// 回执还可能丢在路上。只重发撤销是问不出结局的——交易所会说「不认识这张单」，
    /// 而它可能早就成交或者早就撤掉了。所以撤完必须再核对一遍，由交易所给出终态
    /// （第 9.2 节）。
    ///
    /// 撤销只要库里存着的 `cloid`，不需要先问到订单状态，所以订单读取失败也不影响它。
    /// 两段各自的失败都记在返回值里，由调用方决定这一步能不能宣告收场。
    pub(super) async fn cancel_only(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        now: DateTime<Utc>,
    ) -> CancelOnly {
        let instrument = strategy.spec().instrument().clone();
        let mut canceled = Vec::new();
        let mut cancel_error = None;
        for order in orders.iter_mut().filter(|order| order.record.is_working()) {
            match self.cancel_order(&instrument, order, now).await {
                Ok(true) => {
                    order.updated_at = now;
                    if let Err(error) = self.store.update_order(strategy_id, order) {
                        // 库写不进去是本地的问题，再撤下去也没有意义：撤销结果记不下来，
                        // 下一轮会把已经撤掉的单再撤一遍。
                        cancel_error.get_or_insert(error.into());
                        break;
                    }
                    canceled.push(order.record.client_order_id.clone());
                }
                // 撤销没确认：保持原状，下一次驱动继续尝试。
                Ok(false) => {}
                // 一张单被拒收不该让后面的单跟着不撤：能撤的都要撤掉，撤不掉的下一轮再来。
                Err(error) => {
                    cancel_error.get_or_insert(error);
                }
            }
        }

        let mut applied_fills = 0;
        let mut reconcile_error = None;
        match self
            .reconcile_orders(strategy_id, strategy, state, orders, now)
            .await
        {
            Ok(reconciled) => {
                applied_fills = reconciled.applied_fills;
                if let Err(error) = self.store.save_fills_synced_through(strategy_id, now) {
                    reconcile_error = Some(error.into());
                }
            }
            Err(error) => reconcile_error = Some(error),
        }
        tracing::info!(
            strategy = %strategy_id,
            canceled = canceled.len(),
            applied_fills,
            "cancel-only round done"
        );
        CancelOnly {
            canceled,
            applied_fills,
            cancel_error,
            reconcile_error,
        }
    }

    /// 恢复：暂停之后接着跑（第 9.4 节）。
    ///
    /// 还在运行的策略什么都没做——它从来没停过，挂单还是对的，撤了重挂只会白白丢掉
    /// 排队位置。
    pub async fn resume(&self, strategy_id: &StrategyId) -> Result<ResumeOutcome, RuntimeError> {
        let _guard = self.begin_waiting(strategy_id).await?;
        let now = self.clock.now();
        let status = self.store.load_strategy(strategy_id)?.status;
        match status {
            // 暂停时已经撤过单；撤不掉的订单还可能在场上，等它们过期、再撤一次，
            // 否则恢复之后会重复下单。刚点完暂停还在撤单的也走这里：用户改主意了，
            // 与其等它撤完再恢复，不如现在就按恢复的流程走一遍。
            //
            // 恢复中的也走这里：用户点「恢复」就是让后台立刻再试一次，而不是等下一轮。
            //
            // 三条路都先落到「恢复中」：用户点的恢复和常驻服务的启动恢复走的是同一条
            // 规则——遗留订单收干净之前一张新单都不下（[`Runtime::resume_inner`]）。
            StrategyStatus::Paused | StrategyStatus::Pausing | StrategyStatus::Restoring => {
                self.store
                    .save_strategy_status(strategy_id, StrategyStatus::Restoring, now)?;
                self.resume_inner(strategy_id, now).await
            }
            StrategyStatus::Running => Ok(ResumeOutcome::AlreadyRunning),
            StrategyStatus::Stopped => Err(RuntimeError::Archived(strategy_id.clone())),
            // 已经点了停止：撤单收尾由常驻服务推进，不再接受恢复。
            StrategyStatus::Stopping | StrategyStatus::Draft => {
                Err(RuntimeError::NotRunning(strategy_id.clone(), status))
            }
        }
    }

    /// 进程重启之后要恢复的策略：状态还是「运行中」或「恢复中」的（第 12 节）。
    ///
    /// 常驻服务启动时把它们逐个落到「恢复中」，再由驱动循环一轮一轮推进。
    pub fn to_restore(&self) -> Result<Vec<StrategyId>, RuntimeError> {
        self.with_status(&[StrategyStatus::Running, StrategyStatus::Restoring])
    }

    /// 把策略落到「恢复中」：重启前挂出去的单还没撤，先走一步就会在旧单之上再挂一遍。
    ///
    /// 「恢复中」不下任何新单，网页上看得到它和恢复的进展；驱动循环每一轮继续试，
    /// 成功之后自己转成「运行中」。一个策略恢复不成功不影响别的策略。
    pub async fn hold_for_restore(
        &self,
        strategy_id: &StrategyId,
        note: &str,
    ) -> Result<(), RuntimeError> {
        let _guard = self.begin_waiting(strategy_id).await?;
        let now = self.clock.now();
        self.store
            .save_strategy_status(strategy_id, StrategyStatus::Restoring, now)?;
        self.annotate_view(strategy_id, StrategyStatus::Restoring, note.to_string())
            .await
    }

    /// 常驻循环每一轮要驱动的策略（第 12 节）。
    ///
    /// 除了运行中的，还有「恢复中」「暂停中」「停止中」：恢复中还没把遗留订单收干净，
    /// 暂停中/停止中还没撤干净，撤单、按 `cloid` 查终态、补记成交都靠驱动推进。
    /// 不驱动它们，恢复、暂停和停止就永远停在半路上。
    pub fn drivable(&self) -> Result<Vec<StrategyId>, RuntimeError> {
        self.with_status(&[
            StrategyStatus::Running,
            StrategyStatus::Restoring,
            StrategyStatus::Pausing,
            StrategyStatus::Stopping,
        ])
    }

    /// 恢复中的策略走一步：先把遗留订单收干净，收干净了就转成运行中接着跑。
    ///
    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    ///
    /// 一轮只做能做完的那部分：遗留订单还没过期、还没撤掉就什么都不做，等下一轮。常驻循环
    /// 每几秒一轮，比在恢复里睡一觉更好——睡着的时候别的策略也驱动不了。
    pub(super) async fn restore_step(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        match self.resume_inner(strategy_id, now).await? {
            ResumeOutcome::Resumed(report) => Ok(report),
            // 这一轮什么都没做成：状态留在「恢复中」，视图上已经写清楚了在等什么。
            ResumeOutcome::WaitingForExpiry { .. } | ResumeOutcome::WaitingForSettle { .. } => {
                Ok(self.nothing_happened(strategy_id, StrategyStatus::Restoring))
            }
            ResumeOutcome::AlreadyRunning => Err(RuntimeError::NotRunning(
                strategy_id.clone(),
                StrategyStatus::Restoring,
            )),
        }
    }

    /// 一步什么都没做时拼出的报告。
    pub(super) fn nothing_happened(
        &self,
        strategy_id: &StrategyId,
        status: StrategyStatus,
    ) -> StepReport {
        StepReport {
            strategy_id: strategy_id.clone(),
            status,
            plan: Plan::default(),
            placed: Vec::new(),
            canceled: Vec::new(),
            applied_fills: 0,
            position: Position::ZERO,
            expected_position: Position::ZERO,
        }
    }

    /// 恢复：等遗留订单过期、撤销它们、按 `cloid` 核对出终态、补记成交，收干净了才
    /// 转成「运行中」重新挂单。
    ///
    /// 用户点的恢复和常驻服务的启动恢复都走这里：两条路要的规则是一样的——遗留订单还在场
    /// 时就开始下单，同一个单位就可能有两张都会成交的单。收不干净时状态留在「恢复中」，
    /// 由驱动循环一轮一轮推进（[`Runtime::drivable`]）。
    ///
    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权，并把它落到「恢复中」。
    pub(super) async fn resume_inner(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<ResumeOutcome, RuntimeError> {
        match self.resume_once(strategy_id, now).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                // 恢复的那一段没走通（交易所连不上、账户查不到）：状态留在「恢复中」，
                // 网页上看得到原因，下一轮再试。
                //
                // 但状态已经不是「恢复中」时说明遗留订单已经收干净、策略已经转成运行中，
                // 这就是普通的一步失败，不该把它退回「恢复中」——那会让它再撤一遍刚挂
                // 出去的单。
                if self.store.load_strategy(strategy_id)?.status == StrategyStatus::Restoring {
                    self.annotate_view(
                        strategy_id,
                        StrategyStatus::Restoring,
                        format!("恢复失败：{error}。后台会继续重试。"),
                    )
                    .await?;
                }
                Err(error)
            }
        }
    }

    pub(super) async fn resume_once(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<ResumeOutcome, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let mut state = self.load_state(strategy_id, &strategy)?;
        let mut orders = self.store.load_orders(strategy_id)?;

        // 1. 还有可能落地的订单：等它们过期，否则可能重复下单。
        let pending = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.expires_at)
            .max();
        if let Some(until) = pending.filter(|until| *until > now) {
            tracing::info!(strategy = %strategy_id, %until, "waiting for order expiry before resuming");
            self.annotate_view(
                strategy_id,
                StrategyStatus::Restoring,
                format!("等重启前挂出去的单过期（{until}）之后再重新挂单。"),
            )
            .await?;
            return Ok(ResumeOutcome::WaitingForExpiry { until });
        }

        // 2. 只撤单的一轮：先补拉成交、核对挂单，再撤销遗留挂单、按 `cloid` 核对终态。
        //
        //    撤销只把它变成终态，成交还没有核对：撤销前可能已经有成交、成交记录也可能
        //    还没到。这些订单继续保持 `fills_reconciled = false`，补拉成交的回查起点才会
        //    一直追到它们挂出去的时间，不会漏掉成交。
        //
        //    查询失败也照撤：撤哪几张单只靠库里存着的 `cloid`（[`Runtime::cancel_only_round`]）。
        //    以前这里先做一次同步、失败就直接返回，结果是成交流水或挂单列表一直查不到时，
        //    遗留挂单一轮都不撤，它们继续留在交易所上。
        let CancelOnlyRound {
            applied_fills,
            synced,
            cancel_error,
            ..
        } = self
            .cancel_only_round(
                strategy_id,
                &stored,
                &strategy,
                &mut state,
                &mut orders,
                now,
            )
            .await?;
        // 这一轮有没走通的地方：状态留在「恢复中」，把原因报出去（`resume_inner` 会写进
        // 视图），下一轮再试。撤销本身照常做了，已经撤掉的单不会因为这一步报错而重挂。
        //
        // 同步出错时更不能往下走：内存里的格子和订单可能停在没落盘的那一份上，按它判定
        // 出来的「该下什么单」是错的。同步的结果本身这里不用——第 4 步的读数会重新取一遍。
        synced?;
        if let Some(error) = cancel_error {
            return Err(error);
        }

        // 3. 没收场就不进「运行中」：撤销没确认、终态没核对出来，留下的都是还会成交的单。
        //    状态留在「恢复中」，后台下一轮继续推进。
        if !leftovers_settled(&orders) {
            let working = orders
                .iter()
                .filter(|order| order.record.is_working())
                .count();
            tracing::info!(
                strategy = %strategy_id,
                working,
                "the leftover orders are not settled yet, staying in restoring"
            );
            self.annotate_view(strategy_id, StrategyStatus::Restoring, settle_note(&orders))
                .await?;
            return Ok(ResumeOutcome::WaitingForSettle { working });
        }

        // 4. 收干净了：转「运行中」，按状态重新挂单。补拉成交、仓位核对和判定都在这一步
        //    里——仓位对不上时它照常暂停，不会先下出一批单。
        self.store
            .save_strategy_status(strategy_id, StrategyStatus::Running, now)?;
        let mut report = self.running_step(strategy_id, now).await?;
        // 这一步的读数重新取过一遍，只有成交笔数要算上收尾时补记的那几笔：报告说的是
        // 「这次恢复一共记下了多少笔成交」，收尾补的和重新挂单时补的都是这次恢复做的。
        report.applied_fills += applied_fills;
        Ok(ResumeOutcome::Resumed(report))
    }
}

/// 只撤单的一步是否已经收尾：没有在场订单，而且每张订单的成交都对完了账。
///
/// 只看「没有在场订单」是不够的：撤销只把订单变成终态，撤销前可能已经有成交、成交
/// 记录也可能还没到。这时候归档，那段成交就再也没人补记了（第 9.2 节）。
///
/// 代价是：成交流水始终不到齐时，策略会一直停在「暂停中」「停止中」。这正是要的——
/// 停在半路能被看见，静默归档会把差额藏起来。
pub(super) fn cancel_only_settled(orders: &[StoredOrder]) -> bool {
    orders
        .iter()
        .all(|order| !order.record.is_working() && order.fills_reconciled)
}

/// 恢复的遗留订单是不是都收场了：没有还可能在场的订单。
///
/// 不看成交对没对完账：恢复之后策略继续跑，流水还会再来，这一格的预留由规划按
/// `FillsPending` 挡住，不会按旧数量补单。归档不一样——归档之后没有人再驱动这个策略，
/// 所以「停止中」必须等到流水到齐（[`cancel_only_settled`]）。
///
/// 结果从来没拿到过的订单（`Unknown`、`Submitting`）也算在场。交易所对它们只会回答
/// 「查无此单」，撤销问不出结局（第 9.2 节）：它可能已经成交，这时候重新挂单，同一个
/// 单位上就会多出一张会成交的单。预留一直保留，等三方核对做完再放行（任务 08）。
pub(super) fn leftovers_settled(orders: &[StoredOrder]) -> bool {
    orders.iter().all(|order| !order.record.is_working())
}

/// 这张订单的结果我们从来没拿到过。
///
/// 下单超时（`Unknown`）和写库之后、发送之前崩溃（`Submitting`）都是这一类：交易所可能
/// 压根没收到这张单，撤销只会得到「查无此单」。
pub(super) fn outcome_unknown(status: OrderStatus) -> bool {
    matches!(status, OrderStatus::Unknown | OrderStatus::Submitting)
}

/// 恢复中为什么还没转成「运行中」，写给用户看的一句话。
///
/// 两种原因分开说：撤销没确认的要重试；结果从来没拿到过的，重试也问不出结局——交易所只会
/// 回答「查无此单」，得等三方核对（任务 08）才能放行。混在一起说「正在重试」，用户会以为
/// 等下去就能好。
pub(super) fn settle_note(orders: &[StoredOrder]) -> String {
    let unknown = orders
        .iter()
        .filter(|order| order.record.is_working() && outcome_unknown(order.record.status))
        .count();
    let working = orders
        .iter()
        .filter(|order| order.record.is_working() && !outcome_unknown(order.record.status))
        .count();
    match (working, unknown) {
        (0, count) => format!(
            "还有 {count} 张遗留订单的结果没查出来（交易所只回答「查无此单」），撤销问不出结局，\
             预留先留着；后台会继续按 cloid 核对。"
        ),
        (count, 0) => format!("还有 {count} 张遗留订单没撤掉，正在重试。"),
        (working, unknown) => format!(
            "还有 {working} 张遗留订单没撤掉、{unknown} 张的结果没查出来；后台会继续撤、继续核对。"
        ),
    }
}

/// 「暂停中」「停止中」为什么还没走完，写给用户看的一句话。
///
/// 只说还差什么：撤单撤不掉要重试，成交没对完账要等流水。没有这两种情况时是 `None`——
/// 那一刻状态马上就会转成「已暂停」或「已归档」，不需要解释。
pub(super) fn cancel_only_note(orders: &[StoredOrder], status: StrategyStatus) -> Option<String> {
    let working = orders
        .iter()
        .filter(|order| order.record.is_working())
        .count();
    let unreconciled = orders
        .iter()
        .filter(|order| !order.record.is_working() && !order.fills_reconciled)
        .count();
    let done = if status == StrategyStatus::Pausing {
        "暂停"
    } else {
        "归档"
    };
    match (working, unreconciled) {
        (0, 0) => None,
        (0, count) => Some(format!("还有 {count} 张订单的成交没对完账，对完才{done}。")),
        (count, 0) => Some(format!("还有 {count} 张订单没撤掉，正在重试。")),
        (working, unreconciled) => Some(format!(
            "还有 {working} 张订单没撤掉、{unreconciled} 张订单的成交没对完账。"
        )),
    }
}

/// 只撤单的步骤没拿到展示读数时的说明。
///
/// 读数沿用上一份，用户看到的可能是一份几分钟前的行情；不写清楚原因，它看起来和
/// 刚取回来的没什么两样。撤单和成交对账没受影响，也要说一句，免得用户以为整步都失败了。
pub(super) fn stale_reading_note(what: &str, progress: Option<String>) -> String {
    let stale = format!("这一步没读到{what}，读数沿用上一份。");
    match progress {
        Some(progress) => format!("{stale}{progress}"),
        None => stale,
    }
}
