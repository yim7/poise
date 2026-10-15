//! 运行时：唯一写入入口。
//!
//! 负责把纯计算的策略接到真实世界上：订单先写数据库再发送、按成交记账、
//! 核对仓位、进程重启后恢复（第 9.1、9.4、12 节）。
//!
//! 一个策略由「一步」驱动（[`Runtime::step`]）：与交易所核对在场订单、补记成交、
//! 核对仓位、规划、撤单、下单。什么时候走这一步由调用方决定，测试里直接调用。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use poise_strategy::Strategy;
use poise_strategy::config::{ConfigError, StrategyConfig};
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::order::{ClientOrderId, OrderRecord, OrderStatus, Side, WorkingOrder};
use poise_strategy::planning::{Plan, WaitReason};
use poise_strategy::state::{StateError, StrategyState};
use poise_strategy::units::{Position, Quantity};

use crate::exchange::{Exchange, ExchangeError, ExchangeOrder, OrderLookup, OrderRequest};
use crate::store::{
    Store, StoreError, StoredFill, StoredOrder, StoredStrategy, StrategyId, StrategyStatus,
};

/// 运行参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeOptions {
    /// 下单的 `expiresAfter`：发送时刻加几秒（第 9.1 节）。
    ///
    /// 超过这个时间戳的动作会被交易所拒绝，因此它可以用来判定一张结果未知的
    /// 订单还会不会落地。
    pub order_expires_after: Duration,
    /// 每次拉成交时往前多查一段时间。
    ///
    /// 成交推送和按时间查询都可能有延迟：一笔成交的时间戳可能早于上一次的同步点，
    /// 只按同步点查询就会永久漏掉它。多查一段是安全的，因为重复的成交会被去重。
    /// 具体长度需要在测试网确认（第 9.2 节）。
    pub fills_lookback: Duration,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            order_expires_after: Duration::seconds(5),
            fills_lookback: Duration::seconds(60),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
    #[error("strategy config is invalid: {0}")]
    Config(#[from] ConfigError),
    #[error("strategy state is invalid: {0}")]
    State(#[from] StateError),
    #[error("strategy `{0}` is not running but {1:?}")]
    NotRunning(StrategyId, StrategyStatus),
    /// 这个策略正在被另一个调用驱动。同一个策略同一时刻只能走一步。
    #[error("strategy `{0}` is already being driven")]
    Busy(StrategyId),
}

/// 走一步的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    pub strategy_id: StrategyId,
    pub status: StrategyStatus,
    /// 这一步的规划结果。
    pub plan: Plan,
    /// 这一步尝试发送的订单，含被拒绝和结果未知的。
    pub placed: Vec<ClientOrderId>,
    pub canceled: Vec<ClientOrderId>,
    /// 新记入的成交笔数。
    pub applied_fills: usize,
    /// 仓位核对时交易所的实际仓位。
    pub position: Position,
    /// 仓位核对时我们预计交易所会有的仓位：由格子状态算出的应有持仓，
    /// 加上交易所已经成交、我们还没记入的数量。
    pub expected_position: Position,
}

impl StepReport {
    pub fn is_paused(&self) -> bool {
        self.status == StrategyStatus::Paused
    }
}

/// 恢复流程的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// 还有可能落地的订单，等到这个时间点之后再来。
    WaitingForExpiry { until: DateTime<Utc> },
    /// 已经撤销遗留挂单并走了一步；这一步也可能又把它暂停了。
    Resumed(StepReport),
}

pub struct Runtime {
    exchange: Arc<dyn Exchange>,
    store: Store,
    options: RuntimeOptions,
    /// 正在被驱动的策略。同一个策略并发走两步会用同一份状态各下一遍单。
    stepping: Mutex<HashSet<StrategyId>>,
}

/// 一次驱动期间持有的占用标记，离开作用域就释放。
struct StepGuard<'a> {
    stepping: &'a Mutex<HashSet<StrategyId>>,
    strategy_id: StrategyId,
}

impl Drop for StepGuard<'_> {
    fn drop(&mut self) {
        self.stepping
            .lock()
            .expect("stepping lock poisoned")
            .remove(&self.strategy_id);
    }
}

impl Runtime {
    pub fn new(exchange: Arc<dyn Exchange>, store: Store) -> Self {
        Self::with_options(exchange, store, RuntimeOptions::default())
    }

    pub fn with_options(
        exchange: Arc<dyn Exchange>,
        store: Store,
        options: RuntimeOptions,
    ) -> Self {
        Self {
            exchange,
            store,
            options,
            stepping: Mutex::new(HashSet::new()),
        }
    }

    /// 取得一个策略的驱动权；已经有调用在驱动时返回 [`RuntimeError::Busy`]。
    fn begin(&self, strategy_id: &StrategyId) -> Result<StepGuard<'_>, RuntimeError> {
        let mut stepping = self.stepping.lock().expect("stepping lock poisoned");
        if !stepping.insert(strategy_id.clone()) {
            return Err(RuntimeError::Busy(strategy_id.clone()));
        }
        drop(stepping);
        Ok(StepGuard {
            stepping: &self.stepping,
            strategy_id: strategy_id.clone(),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn options(&self) -> &RuntimeOptions {
        &self.options
    }

    /// 创建并启动一个策略：校验配置、保存状态、写入数据库。
    ///
    /// 只做持久化，不挂单；挂单发生在 [`Runtime::step`]。
    pub fn create_strategy(
        &self,
        config: StrategyConfig,
        spec: InstrumentSpec,
        fees: Fees,
        now: DateTime<Utc>,
    ) -> Result<StrategyId, RuntimeError> {
        // 先校验：不合法就不要落库。
        let strategy = Strategy::new(config, spec.clone(), fees)?;
        let strategy_id = StrategyId::new();
        self.store.insert_strategy(
            &StoredStrategy {
                strategy_id: strategy_id.clone(),
                instrument: spec.instrument().clone(),
                status: StrategyStatus::Running,
                config: strategy.config().clone(),
                spec,
                fees,
                fills_synced_through: now,
                created_at: now,
                updated_at: now,
            },
            &strategy.initial_state(),
        )?;
        tracing::info!(
            strategy = %strategy_id,
            instrument = strategy.spec().instrument().symbol(),
            cells = strategy.grid().len(),
            "strategy created"
        );
        Ok(strategy_id)
    }

    pub fn status(&self, strategy_id: &StrategyId) -> Result<StrategyStatus, RuntimeError> {
        Ok(self.store.load_strategy(strategy_id)?.status)
    }

    pub fn orders(&self, strategy_id: &StrategyId) -> Result<Vec<StoredOrder>, RuntimeError> {
        Ok(self.store.load_orders(strategy_id)?)
    }

    pub fn fills(&self, strategy_id: &StrategyId) -> Result<Vec<StoredFill>, RuntimeError> {
        Ok(self.store.load_fills(strategy_id)?)
    }

    pub fn cell_states(&self, strategy_id: &StrategyId) -> Result<StrategyState, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        let strategy = Strategy::new(stored.config, stored.spec, stored.fees)?;
        self.load_state(strategy_id, &strategy)
    }

    /// 走一步。
    ///
    /// 同一个策略同一时刻只能走一步：并发调用会返回 [`RuntimeError::Busy`]。
    pub async fn step(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        let _guard = self.begin(strategy_id)?;
        self.step_inner(strategy_id, now).await
    }

    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    async fn step_inner(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        if stored.status != StrategyStatus::Running {
            return Err(RuntimeError::NotRunning(strategy_id.clone(), stored.status));
        }
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let instrument = stored.spec.instrument().clone();
        let mut state = self.load_state(strategy_id, &strategy)?;
        let mut orders = self.store.load_orders(strategy_id)?;

        // 1. 交易所现在挂着哪些单。
        let exchange_working = self.exchange.working_orders(&instrument).await?;

        // 2. 补记成交。放在核对挂单之后：这段时间里成交的订单，
        //    成交记录已经可见，不会被误判成撤销。
        let since = self.fills_since(&stored, &orders);
        let mut applied_fills = self
            .sync_fills(strategy_id, &strategy, &mut state, &mut orders, since, now)
            .await?;

        // 3. 收尾不在挂单列表里的订单。
        //
        //    一张单完全成交后同样会消失，所以不能只凭「过了 expiresAfter」判它撤销，
        //    要问交易所它是什么结局（第 9.2 节）。
        //
        //    `reported` 记下交易所告诉我们的累计成交量：成交记录可能比挂单状态晚到，
        //    仓位核对要拿它和我们记入的数量比。
        let mut reported: Vec<(ClientOrderId, Quantity)> = Vec::new();
        let mut unknown_result = false;
        for order in orders.iter_mut().filter(|order| order.record.is_working()) {
            let exchange_view = match exchange_working
                .iter()
                .find(|working| working.client_order_id == order.record.client_order_id)
                .cloned()
            {
                Some(working) => working,
                None => {
                    match self
                        .exchange
                        .order_status(&instrument, &order.record.client_order_id)
                        .await?
                    {
                        OrderLookup::Found(found) => found,
                        // 交易所查不到这张单。凭过期就断定它没有影响是不安全的：
                        // 保持预留、不释放、不重发，并说明原因（第 9.2 节）。
                        OrderLookup::Unknown => {
                            if now >= order.expires_at {
                                unknown_result = true;
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
            let unbooked = unbooked_quantity(
                exchange_view.cumulative_filled,
                order.record.cumulative_filled,
            );

            // 交易所还认它是挂单：按挂单状态更新。
            if exchange_view.status.is_working() {
                if Self::adopt_snapshot(order, &exchange_view, now) {
                    self.store.update_order(strategy_id, order)?;
                }
                continue;
            }

            // 已成交，但成交记录还没到齐：保留预留等它补齐，不释放剩余量、也不重发。
            // （第 9.2.3 节的三项核对属于后续任务。）
            if exchange_view.status == OrderStatus::Filled && !unbooked.is_zero() {
                continue;
            }

            if !unbooked.is_zero() {
                // 撤销或拒绝：先把交易所认到的成交量吸收进格子，再按结局释放剩余量。
                strategy.apply_fill(
                    &mut state,
                    &mut order.record,
                    exchange_view.cumulative_filled,
                );
                order.exchange_order_id = Some(exchange_view.exchange_order_id.clone());
                order.record.status = exchange_view.status;
                order.updated_at = now;
                self.store
                    .save_applied_fills(strategy_id, &state, &[&*order])?;
                continue;
            }

            order.exchange_order_id = Some(exchange_view.exchange_order_id.clone());
            order.record.status = exchange_view.status;
            order.updated_at = now;
            self.store.update_order(strategy_id, order)?;
        }

        // 4. 仓位核对（第 7.1.3 节）。差额能用在场订单已经成交、我们还没记入的
        //    数量解释时不算失败：成交记录可能还没到，等到手后状态自然会对上。
        let mut position = self.exchange.position(&instrument).await?;
        let mut expected_position = Self::expected_position(&strategy, &state, &reported, &orders);
        if position != expected_position {
            // 查成交和查仓位之间可能刚好有成交：补拉一次成交、重新查一次仓位，再核对。
            // 两边都是刚取到的还对不上，才是真的对不上。
            applied_fills += self
                .sync_fills(strategy_id, &strategy, &mut state, &mut orders, since, now)
                .await?;
            position = self.exchange.position(&instrument).await?;
            expected_position = Self::expected_position(&strategy, &state, &reported, &orders);
        }
        if position != expected_position {
            tracing::warn!(
                strategy = %strategy_id,
                actual = %position,
                expected = %expected_position,
                "position check failed, pausing the strategy"
            );
            let canceled = self.pause_inner(strategy_id, now).await?;
            return Ok(StepReport {
                strategy_id: strategy_id.clone(),
                status: StrategyStatus::Paused,
                plan: Plan {
                    waiting: vec![WaitReason::PositionCheckFailed],
                    ..Plan::default()
                },
                placed: Vec::new(),
                canceled,
                applied_fills,
                position,
                expected_position,
            });
        }

        // 5. 规划。
        let market_price = self.exchange.mark_price(&instrument).await?;
        let working_views: Vec<WorkingOrder> = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.record.working_view())
            .collect();
        let mut plan = strategy.plan(&state, &working_views, market_price);
        if unknown_result {
            plan.waiting.push(WaitReason::OrderResultUnknown);
        }

        // 6. 撤单。撤不掉的订单保留预留；有撤单结果不确定时这一步不下任何新单，
        //    避免同一侧出现两张都会成交的订单。第 3 步里结果未知的订单同样保住了
        //    预留，规划时它仍然算在场订单，所以也不会为同一格重复下单。
        //    （第 7.2 节的系统状态闸门——有订单结果未知时本策略不发任何新订单——
        //    属于后续任务。）
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
                .cancel_order(&instrument, &mut orders[index], now)
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

        // 7. 下单：先写数据库，再发送（第 9.1 节）。
        let mut placed = Vec::new();
        let mut intents: &[poise_strategy::planning::OrderIntent] = &plan.place;
        if uncertain {
            plan.waiting.push(WaitReason::OrderResultUnknown);
            intents = &[];
        }
        for intent in intents {
            let client_order_id = new_client_order_id();
            let expires_at = now + self.options.order_expires_after;
            let mut order = StoredOrder {
                record: OrderRecord {
                    client_order_id: client_order_id.clone(),
                    cell_index: intent.cell_index,
                    purpose: intent.purpose,
                    limit_price: intent.limit_price,
                    quantity: intent.quantity,
                    cumulative_filled: Quantity::ZERO,
                    reduce_only: intent.reduce_only,
                    status: OrderStatus::Submitting,
                },
                exchange_order_id: None,
                expires_at,
                placed_at: now,
                updated_at: now,
            };
            self.store.insert_order(strategy_id, &order)?;

            let request = OrderRequest {
                instrument: instrument.clone(),
                client_order_id: client_order_id.clone(),
                side: intent.side,
                limit_price: intent.limit_price,
                quantity: intent.quantity,
                reduce_only: intent.reduce_only,
                expires_at,
            };
            match self.exchange.place_order(&request).await {
                Ok(receipt) => {
                    order.exchange_order_id = Some(receipt.exchange_order_id);
                    order.record.status = receipt.status;
                    if !receipt.cumulative_filled.is_zero() {
                        // 下单即成交：当场记账，避免这一步结束时仓位对不上。
                        // 格子状态和订单记录写在同一个事务里。
                        strategy.apply_fill(
                            &mut state,
                            &mut order.record,
                            receipt.cumulative_filled,
                        );
                        self.store
                            .save_applied_fills(strategy_id, &state, &[&order])?;
                        orders.push(order);
                        placed.push(client_order_id);
                        continue;
                    }
                }
                Err(ExchangeError::Rejected { reason }) => {
                    order.record.status = OrderStatus::Rejected;
                    tracing::warn!(
                        strategy = %strategy_id,
                        client_order_id = %client_order_id,
                        reason = %reason,
                        "order rejected"
                    );
                }
                Err(error) => {
                    order.record.status = OrderStatus::Unknown;
                    tracing::warn!(
                        strategy = %strategy_id,
                        client_order_id = %client_order_id,
                        error = %error,
                        "order result unknown"
                    );
                }
            }
            self.store.update_order(strategy_id, &order)?;
            orders.push(order);
            placed.push(client_order_id);
        }

        // 格子状态在每次记账时就已经和订单记录一起落盘了。
        self.store.save_fills_synced_through(strategy_id, now)?;
        tracing::info!(
            strategy = %strategy_id,
            market_price = %plan.market_price,
            placed = placed.len(),
            canceled = canceled.len(),
            applied_fills,
            position = %position,
            "step done"
        );

        Ok(StepReport {
            strategy_id: strategy_id.clone(),
            status: StrategyStatus::Running,
            plan,
            placed,
            canceled,
            applied_fills,
            position,
            expected_position,
        })
    }

    /// 暂停：撤销全部在场订单、吸收最终成交、状态保留（第 9.4 节）。
    pub async fn pause(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<Vec<ClientOrderId>, RuntimeError> {
        let _guard = self.begin(strategy_id)?;
        self.pause_inner(strategy_id, now).await
    }

    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    async fn pause_inner(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<Vec<ClientOrderId>, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let instrument = stored.spec.instrument().clone();
        let mut state = self.load_state(strategy_id, &strategy)?;
        let mut orders = self.store.load_orders(strategy_id)?;

        // 拉成交的起点要在撤单之前算：撤单会把在场订单变成终态，之后再看就只剩下
        // 同步点这个下限，比订单实际挂出去的时间晚，撤单前的成交会被漏掉。
        let since = self.fills_since(&stored, &orders);

        let mut canceled = Vec::new();
        for order in orders.iter_mut().filter(|order| order.record.is_working()) {
            // 撤不掉的订单保持原状，下次继续尝试。
            if self.cancel_order(&instrument, order, now).await? {
                order.updated_at = now;
                self.store.update_order(strategy_id, order)?;
                canceled.push(order.record.client_order_id.clone());
            }
        }

        let applied_fills = self
            .sync_fills(strategy_id, &strategy, &mut state, &mut orders, since, now)
            .await?;
        self.store.save_fills_synced_through(strategy_id, now)?;
        self.store
            .save_strategy_status(strategy_id, StrategyStatus::Paused, now)?;
        tracing::warn!(
            strategy = %strategy_id,
            canceled = canceled.len(),
            applied_fills,
            "strategy paused"
        );
        Ok(canceled)
    }

    /// 恢复：先等 `expiresAfter` 过期，再撤销遗留挂单，然后按保存的状态重新挂单。
    ///
    /// 进程重启后用这个入口（第 12 节）；暂停后用户修好仓位也可以用它继续（第 9.4 节）。
    /// 永远不会重发旧订单：新订单都用新的 `cloid`。
    pub async fn resume(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<ResumeOutcome, RuntimeError> {
        let _guard = self.begin(strategy_id)?;
        let stored = self.store.load_strategy(strategy_id)?;
        let instrument = stored.spec.instrument().clone();
        let mut orders = self.store.load_orders(strategy_id)?;

        // 1. 还有可能落地的订单：等它们过期，否则可能重复下单。
        let pending = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.expires_at)
            .max();
        if let Some(until) = pending.filter(|until| *until > now) {
            tracing::info!(strategy = %strategy_id, %until, "waiting for order expiry before resuming");
            return Ok(ResumeOutcome::WaitingForExpiry { until });
        }

        // 2. 撤销遗留挂单。撤不掉的订单保持在场，下一步的规划会再撤一次。
        for order in orders.iter_mut().filter(|order| order.record.is_working()) {
            if self.cancel_order(&instrument, order, now).await? {
                order.updated_at = now;
                self.store.update_order(strategy_id, order)?;
            }
        }

        // 3. 按状态重新挂单：走一步会补拉成交、核对仓位，再重新规划。
        self.store
            .save_strategy_status(strategy_id, StrategyStatus::Running, now)?;
        let report = self.step_inner(strategy_id, now).await?;
        Ok(ResumeOutcome::Resumed(report))
    }

    /// 按交易所给的订单状态更新本地记录，返回是否有变化。
    fn adopt_snapshot(
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

    /// 仓位核对时我们预计交易所会有的仓位。
    ///
    /// 由格子状态算出的应有持仓，加上交易所已经成交、我们还没记入的数量：
    /// 成交记录可能比挂单状态晚到，这段差额是能解释的（第 7.1.3 节）。
    fn expected_position(
        strategy: &Strategy,
        state: &StrategyState,
        reported: &[(ClientOrderId, Quantity)],
        orders: &[StoredOrder],
    ) -> Position {
        let mut unrecorded = 0_i64;
        for (client_order_id, cumulative) in reported {
            let Some(order) = orders
                .iter()
                .find(|order| &order.record.client_order_id == client_order_id)
            else {
                continue;
            };
            // `reported` 是第 3 步的快照。补拉成交之后我们的记录可能已经追上甚至
            // 超过它，那时差额按 0 算，而不是负数。
            let unbooked = unbooked_quantity(*cumulative, order.record.cumulative_filled);
            unrecorded += signed_delta(order.record.purpose.side(), unbooked);
        }
        Position::from_units(strategy.expected_position(state).units() + unrecorded)
    }

    /// 拉成交的起点。
    ///
    /// 从上次同步点往前多查一段，免得时间戳早于同步点的成交被永久漏掉；如果还有
    /// 在场订单比这更早，就从最早的在场订单查起，保证每张在场订单的成交记录都是
    /// 完整的——判定一张查不到的单「没有任何影响」时要靠这个完整性（第 9.2 节）。
    fn fills_since(&self, stored: &StoredStrategy, orders: &[StoredOrder]) -> DateTime<Utc> {
        let watermark = stored.fills_synced_through - self.options.fills_lookback;
        orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.placed_at)
            .min()
            .map_or(watermark, |earliest| watermark.min(earliest))
    }

    /// 补拉成交并按 `cloid` 记账。
    ///
    /// 是否记账只看订单的累计成交量（第 9.3 节），不看这条成交流水是否已经存在：
    /// 流水先写、状态后写，中间崩溃时成交不会被吞掉，重复拉到的成交也不会记两次。
    /// 状态和订单记录在同一个事务里落盘，所以不会出现「格子记了、订单没记」的中间态。
    async fn sync_fills(
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
            // 累计成交量没有变大：重复、乱序或累计回退的成交，不改变状态。
            if fill.cumulative_quantity <= order.record.cumulative_filled {
                continue;
            }
            if strategy
                .apply_fill(state, &mut order.record, fill.cumulative_quantity)
                .is_duplicate()
            {
                continue;
            }
            applied += 1;
            order.updated_at = now;
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
    async fn cancel_order(
        &self,
        instrument: &Instrument,
        order: &mut StoredOrder,
        now: DateTime<Utc>,
    ) -> Result<bool, RuntimeError> {
        let client_order_id = order.record.client_order_id.clone();
        let outcome = self
            .exchange
            .cancel_order(instrument, &client_order_id)
            .await;
        match outcome {
            Ok(()) => {
                order.record.status = terminal_status(&order.record);
                Ok(true)
            }
            // 交易所不认识这张单：它可能已经成交、可能早就撤销、也可能压根没落地。
            // 「查不到」分不出是哪一种，成交记录也可能还没到，所以即使过了
            // `expiresAfter` 也保留预留、不释放（第 9.2 节）。完整的三项核对属于后续任务。
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

    fn load_state(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
    ) -> Result<StrategyState, RuntimeError> {
        let state = StrategyState::new(self.store.load_cell_states(strategy_id)?);
        strategy.validate_state(&state)?;
        Ok(state)
    }
}

/// 一笔成交对仓位的贡献，以 1e-8 标的为单位：买为正，卖为负。
fn signed_delta(side: Side, quantity: Quantity) -> i64 {
    match side {
        Side::Buy => quantity.units(),
        Side::Sell => -quantity.units(),
    }
}

/// 交易所报的累计成交量减去我们已经记入的数量，即「成交了但还没记账」的部分。
///
/// 已经记入得不比它少时是 0。这里不能用 `Quantity::saturating_sub`：它是 i64 的
/// 饱和减法，减多了会给出负数，而「还差多少没记入」不可能为负。
fn unbooked_quantity(reported: Quantity, recorded: Quantity) -> Quantity {
    if reported > recorded {
        Quantity::from_units(reported.units() - recorded.units())
    } else {
        Quantity::ZERO
    }
}

/// 订单不再在场时的终态：全部成交记成交，其余记撤销。
fn terminal_status(record: &OrderRecord) -> OrderStatus {
    if record.remaining().is_zero() {
        OrderStatus::Filled
    } else {
        OrderStatus::Canceled
    }
}

fn new_client_order_id() -> ClientOrderId {
    ClientOrderId::try_new(format!("0x{}", uuid::Uuid::new_v4().simple()))
        .expect("a generated client order id is never empty")
}
