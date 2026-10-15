//! 运行时：唯一写入入口。
//!
//! 负责把纯计算的策略接到真实世界上：订单先写数据库再发送、按成交记账、
//! 核对仓位、进程重启后恢复（第 9.1、9.4、12 节）。
//!
//! 一个策略由「一步」驱动（[`Runtime::step`]），一步分三段：
//! - **读取**（[`Runtime::read_step`]）：向交易所取数，把成交、挂单状态、仓位和标记价
//!   读成一份快照；
//! - **判定**（[`poise_strategy::decision::judge`]）：纯计算，只看快照就说这一步能不能
//!   下单、要等什么、哪些单位的哪一侧被挡住；
//! - **执行**（[`Runtime::execute_plan`]）：撤单、下单、记账。
//!
//! 什么时候走这一步由调用方决定，测试里直接调用。
//!
//! 时间全部来自注入的时钟（[`crate::clock::Clock`]）：订单的 `expiresAfter` 按实际
//! 发送时刻算，限速按实际成交时刻算（第 4.1、9.1 节）。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use poise_strategy::Strategy;
use poise_strategy::config::StrategyConfig;
use poise_strategy::decision::{
    Decision, OrderState, PauseReason, PositionCheck, PositionReading, Verdict,
    expected_position as expected_from, fills_settled, inconsistent_fill_records, judge,
    pending_units, position_check, unbooked_quantity,
};
use poise_strategy::error::ConfigError;
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::order::{ClientOrderId, OrderRecord, OrderStatus, WorkingOrder};
use poise_strategy::planning::{OrderIntent, PendingUnit, Plan, WaitReason};
use poise_strategy::preview::StartupPreview;
use poise_strategy::state::{StateError, StrategyState};
use poise_strategy::units::{Position, Price, Quantity};

use crate::clock::{Clock, SystemClock};
use crate::exchange::{Exchange, ExchangeError, ExchangeOrder, OrderLookup, OrderRequest};
use crate::store::{
    Store, StoreError, StoredConfigVersion, StoredFill, StoredOrder, StoredStrategy, StrategyId,
    StrategyStatus,
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
    /// 运行中只允许改限速间隔；改了别的参数必须停下来重建（第 2.4 节）。
    #[error(
        "only the throttle interval can be changed while running, but the new config also changes `{field}`"
    )]
    ConfigChangeNotAllowed { field: &'static str },
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

/// 一次订单核对的结果。
struct ReconcileOutcome {
    /// 交易所报的累计成交量快照，供仓位核对使用。
    reported: Vec<(ClientOrderId, Quantity)>,
    /// 是否有订单的结果不明。
    unknown_result: bool,
    /// 这次核对从交易所的订单状态里补记的成交笔数。
    applied_fills: usize,
    /// 成交流水与订单累计成交量对不上的订单（见 [`Runtime::inconsistent_fill_records`]）。
    inconsistent: Option<ClientOrderId>,
}

/// 交易所对一张订单的说法：它的结局、累计成交量，以及流水表里属于它的数量之和。
///
/// 两个数配对使用：交易所报的累计成交量说明「成交了多少」，流水之和说明「这些成交的
/// 记录到齐了没有」（见 [`fills_settled`]）。
struct ExchangeFacts<'a> {
    /// 交易所报的订单状态与累计成交量。
    view: &'a ExchangeOrder,
    /// 流水表里属于这张订单的数量之和。
    saved_fills: Quantity,
    /// 这份订单状态是什么时候拿到的（查询结果的接收时刻）。
    ///
    /// 终态订单补记成交时只能用它当成交时刻的上界：成交发生在查询之前，所以接收时刻
    /// 一定不早于真实成交时刻。这一步开始的时刻不能当上界——它可能早于真实成交时刻，
    /// 记下来之后真实流水也改不回去（[`Strategy::record_fill_time`] 只往早里改）。
    observed_at: DateTime<Utc>,
}

/// 仓位核对最多比几轮。
///
/// 一轮是「刷新成交、挂单状态和仓位，再比一次」。数据还在变的时候每轮读到的都不一样，
/// 必须有个上限；上限之内还没读到连续两轮相同的数据，就说明数据不稳定。
const POSITION_CHECK_ROUNDS: usize = 3;

/// 读取阶段读回来的快照：判定需要的全部输入，加这一步报出的仓位。
struct Snapshot {
    market_price: Price,
    /// 这一轮读到的仓位核对数据：交易所的实际仓位、我们推算的应有仓位，
    /// 以及每张订单交易所报的累计成交量快照。
    reading: PositionReading,
    /// 仓位核对的结论。读取阶段的循环里已经算过，判定直接用，不重算一遍。
    check: PositionCheck,
    /// 是否有订单的结果不明。
    unknown_result: bool,
    /// 成交流水与订单累计成交量对不上的订单。
    inconsistent: Option<ClientOrderId>,
    /// 这一步从交易所补记的成交笔数。
    applied_fills: usize,
}

impl Snapshot {
    /// 这一步不下新单时，按读到的仓位和等待原因拼出报告。
    fn stop(
        &self,
        strategy_id: &StrategyId,
        status: StrategyStatus,
        reason: WaitReason,
        canceled: Vec<ClientOrderId>,
    ) -> StepReport {
        StepReport {
            strategy_id: strategy_id.clone(),
            status,
            plan: Plan {
                market_price: self.market_price,
                waiting: vec![reason],
                ..Plan::default()
            },
            placed: Vec::new(),
            canceled,
            applied_fills: self.applied_fills,
            position: self.reading.actual,
            expected_position: self.reading.expected,
        }
    }
}

/// 执行阶段报出的两个列表。
struct Executed {
    placed: Vec<ClientOrderId>,
    canceled: Vec<ClientOrderId>,
}

/// 撤单的结果。
struct Cancels {
    canceled: Vec<ClientOrderId>,
    /// 是否有订单的撤销结果不确定。
    uncertain: bool,
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
    /// 当前时间的来源。策略是纯计算，时间只在这里取。
    clock: Arc<dyn Clock>,
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
        Self::with_clock(exchange, store, options, Arc::new(SystemClock))
    }

    /// 用给定的时钟建运行时。测试注入可控时钟，时间就不再依赖跑得多快。
    pub fn with_clock(
        exchange: Arc<dyn Exchange>,
        store: Store,
        options: RuntimeOptions,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            exchange,
            store,
            options,
            clock,
            stepping: Mutex::new(HashSet::new()),
        }
    }

    /// 当前时间。
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
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
    ) -> Result<StrategyId, RuntimeError> {
        let now = self.clock.now();
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

    /// 保存的策略状态：批次状态与格子状态。
    pub fn state(&self, strategy_id: &StrategyId) -> Result<StrategyState, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        let strategy = Strategy::new(stored.config, stored.spec, stored.fees)?;
        self.load_state(strategy_id, &strategy)
    }

    /// 启动预览：批次表与启动后会立即成交的订单（第 4.6 节）。
    ///
    /// 不落库、不下单，只按交易所当前的标记价算一遍，供网页在确认启动前显示。
    pub async fn preview_startup(
        &self,
        config: StrategyConfig,
        spec: InstrumentSpec,
        fees: Fees,
    ) -> Result<StartupPreview, RuntimeError> {
        let strategy = Strategy::new(config, spec, fees)?;
        let market_price = self
            .exchange
            .mark_price(strategy.spec().instrument())
            .await?;
        Ok(strategy.startup_preview(market_price))
    }

    /// 走一步。
    ///
    /// 同一个策略同一时刻只能走一步：并发调用会返回 [`RuntimeError::Busy`]。
    pub async fn step(&self, strategy_id: &StrategyId) -> Result<StepReport, RuntimeError> {
        let _guard = self.begin(strategy_id)?;
        self.step_inner(strategy_id, self.clock.now()).await
    }

    /// 运行中修改参数：目前只有限速间隔可以改（第 2.4 节）。
    ///
    /// 新配置按策略的标的与费率规整后，除了限速间隔必须和现有配置完全一致：改价格
    /// 范围、比例、批数或网格间距都会让已有的批次划分和格子状态对不上，必须停下来
    /// 重建。改动立刻生效——下一步读的就是新配置——并记一个新的参数版本。
    ///
    /// 加上限速也是改同一个参数：放行进度按修改这一刻重建，限速从这一刻开始计时。
    /// 已经开始的单位保持放行，还没开始的单位回到未放行——它们在场上的订单留在原地，
    /// 等重新放行之后再由规划处理（第 4.1 节）。
    ///
    /// 改配置和走一步都会读写同一份状态，所以两者互斥：正在被驱动时返回
    /// [`RuntimeError::Busy`]。
    pub fn update_config(
        &self,
        strategy_id: &StrategyId,
        config: StrategyConfig,
    ) -> Result<i64, RuntimeError> {
        let _guard = self.begin(strategy_id)?;
        let now = self.clock.now();
        let stored = self.store.load_strategy(strategy_id)?;
        let candidate = Strategy::new(config, stored.spec.clone(), stored.fees)?;
        let new_config = candidate.config().clone();
        if let Some(field) = changed_field(&stored.config, &new_config) {
            return Err(RuntimeError::ConfigChangeNotAllowed { field });
        }
        if stored.config == new_config {
            // 一个参数都没变（比如网页上连点两次保存）：不记新版本，报当前版本号。
            // 参数版本是事后复盘「哪次改动让成交变了」的依据，同样的参数记两遍会让
            // 版本号不再对应不同的参数集。
            return Ok(self.store.latest_config_version(strategy_id)?);
        }
        let adding_throttle = stored.config.long.throttle_interval.is_none()
            && new_config.long.throttle_interval.is_some();
        // 限速进度不属于配置，加上限速时要单独重建：按修改这一刻重建放行进度。它和新
        // 配置写在同一笔里——分成两次写，中间崩掉就留下「配置已经限速、进度还是全部
        // 放行」，重启后限速静默失效（第 4.1 节）。
        let state = if adding_throttle {
            let mut state = self.load_state(strategy_id, &candidate)?;
            candidate.rebuild_throttle(&mut state, now);
            Some(state)
        } else {
            None
        };
        let version = self
            .store
            .save_config(strategy_id, &new_config, state.as_ref(), now)?;
        tracing::info!(
            strategy = %strategy_id,
            version,
            throttle_interval = ?new_config.long.throttle_interval,
            "config updated"
        );
        Ok(version)
    }

    /// 一个策略的参数版本，按版本号从小到大。
    pub fn config_versions(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Vec<StoredConfigVersion>, RuntimeError> {
        Ok(self.store.load_config_versions(strategy_id)?)
    }

    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    ///
    /// 一步分三段：读取（[`Runtime::read_step`]）、判定（[`judge`]）、执行
    /// （[`Runtime::execute_plan`]）。判定是纯计算，所以「这一步该不该继续下单」
    /// 可以直接用数据测试，不用连交易所。
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
        let mut state = self.load_state(strategy_id, &strategy)?;
        let mut orders = self.store.load_orders(strategy_id)?;

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
        let decision = judge(
            snapshot.check,
            snapshot.inconsistent.as_ref(),
            &order_states(&orders),
        );
        if let Some(report) = self
            .stop_if_blocked(strategy_id, &snapshot, &decision, now)
            .await?
        {
            return Ok(report);
        }

        let mut plan = self.plan_step(
            &strategy,
            &state,
            &orders,
            &decision.blocked,
            snapshot.market_price,
            snapshot.unknown_result,
        );
        let executed = self
            .execute_plan(
                strategy_id,
                &strategy,
                &mut state,
                &mut orders,
                &mut plan,
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

        Ok(StepReport {
            strategy_id: strategy_id.clone(),
            status: StrategyStatus::Running,
            plan,
            placed: executed.placed,
            canceled: executed.canceled,
            applied_fills: snapshot.applied_fills,
            position: snapshot.reading.actual,
            expected_position: snapshot.reading.expected,
        })
    }

    /// 判定说要停下来时，收尾并返回报告；可以继续下单时返回 `None`。
    ///
    /// 等待只是「这一步的数据说明不了问题」，状态不变、下一步再核对；暂停是「继续自动
    /// 重试没有意义」，撤销全部挂单交给用户处理（第 9.2 节）。
    async fn stop_if_blocked(
        &self,
        strategy_id: &StrategyId,
        snapshot: &Snapshot,
        decision: &Decision,
        now: DateTime<Utc>,
    ) -> Result<Option<StepReport>, RuntimeError> {
        match decision.verdict {
            Verdict::Proceed => Ok(None),
            Verdict::Wait(reason) => {
                // 每一轮读到的数据都不一样：还在变，这一轮说明不了问题。
                tracing::info!(
                    strategy = %strategy_id,
                    actual = %snapshot.reading.actual,
                    expected = %snapshot.reading.expected,
                    "position data is still moving, checking again next step"
                );
                Ok(Some(snapshot.stop(
                    strategy_id,
                    StrategyStatus::Running,
                    reason,
                    Vec::new(),
                )))
            }
            Verdict::Pause(reason) => {
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
                let canceled = self.pause_inner(strategy_id, now).await?;
                Ok(Some(snapshot.stop(
                    strategy_id,
                    StrategyStatus::Paused,
                    reason.wait_reason(),
                    canceled,
                )))
            }
        }
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
    async fn read_step(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut [StoredOrder],
        now: DateTime<Utc>,
    ) -> Result<Snapshot, RuntimeError> {
        let instrument = stored.spec.instrument();
        let since = self.fills_since(stored, orders);
        let mut applied_fills = self
            .sync_fills(strategy_id, strategy, state, orders, since, now)
            .await?;

        let reconciled = self
            .reconcile_orders(strategy_id, strategy, state, orders, now)
            .await?;
        applied_fills += reconciled.applied_fills;
        let mut reported = reconciled.reported;
        let mut unknown_result = reconciled.unknown_result;
        let mut inconsistent = reconciled.inconsistent;

        self.advance_progress(strategy_id, strategy, state, orders, now)?;

        // 标记价：只进 `Plan` 供展示和日志使用，提前读出来是为了让仓位核对的提前返回
        // 也能报出当时的标记价，而不是一个零值。
        let market_price = self.exchange.mark_price(instrument).await?;

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
            unknown_result |= again.unknown_result;
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
        Ok(Snapshot {
            market_price,
            reading,
            check,
            unknown_result,
            inconsistent,
            applied_fills,
        })
    }

    /// 规划：把判定给出的「被阻塞单位」交给策略，算出这一步要下的单和要撤的单。
    fn plan_step(
        &self,
        strategy: &Strategy,
        state: &StrategyState,
        orders: &[StoredOrder],
        blocked: &[PendingUnit],
        market_price: Price,
        unknown_result: bool,
    ) -> Plan {
        let working_views: Vec<WorkingOrder> = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.record.working_view())
            .collect();
        let mut plan = strategy.plan(state, &working_views, blocked, market_price);
        if unknown_result {
            plan.waiting.push(WaitReason::OrderResultUnknown);
        }
        plan
    }

    /// 执行阶段：撤单、下单、记账。
    ///
    /// 撤不掉的订单保留预留；有撤单结果不确定时这一步不下任何新单，避免同一侧出现两张
    /// 都会成交的订单。读取阶段里结果未知的订单同样保住了预留，规划时它仍然算在场订单，
    /// 所以也不会为同一格重复下单。（第 7.2 节的系统状态闸门——有订单结果未知时本策略
    /// 不发任何新订单——属于后续任务。）
    async fn execute_plan(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
        plan: &mut Plan,
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
            let mut placed = Vec::new();
            for intent in &plan.place {
                // 没发出去的单不进报告：报告说的是这一步做过的动作。
                if let Some(client_order_id) = self
                    .place_one(strategy_id, strategy, state, orders, intent)
                    .await?
                {
                    placed.push(client_order_id);
                }
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
    async fn cancel_planned(
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
    /// 被拒绝和结果未知的订单也照常报出：它们都已经落库，是这一步尝试过的动作。
    /// 返回 `None` 表示这张单这一步根本没发出去（写完库发现有效期已经不够），
    /// 它也没有留在库里。
    async fn place_one(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
        intent: &OrderIntent,
    ) -> Result<Option<ClientOrderId>, RuntimeError> {
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
                return Ok(None);
            }
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
        Ok(Some(client_order_id))
    }

    /// 把订单发出去，按交易所的回答更新本地记录。
    ///
    /// 返回交易所当场报出的累计成交量（下单即成交的数量）；没有成交返回 `None`。
    async fn send_order(
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

    /// 暂停：撤销全部在场订单、吸收最终成交、状态保留（第 9.4 节）。
    pub async fn pause(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Vec<ClientOrderId>, RuntimeError> {
        let _guard = self.begin(strategy_id)?;
        self.pause_inner(strategy_id, self.clock.now()).await
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

        // 撤单成功只说明订单不再在场：撤销前可能已经有成交、成交记录也可能还没到。
        // 这些订单的成交核对没有完成，回查起点继续追到它们挂出去的时间，
        // 恢复时补拉成交才不会漏掉（第 9.2 节）。
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
    pub async fn resume(&self, strategy_id: &StrategyId) -> Result<ResumeOutcome, RuntimeError> {
        let _guard = self.begin(strategy_id)?;
        let now = self.clock.now();
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
        //
        //    撤销只把它变成终态，成交还没有核对：撤销前可能已经有成交、成交记录也可能
        //    还没到。这些订单继续保持 `fills_reconciled = false`，第 3 步补拉成交时
        //    的回查起点才会一直追到它们挂出去的时间，不会漏掉成交。
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

    /// 与交易所核对订单：收尾不在挂单列表里的订单、吸收它们已经成交的部分。
    ///
    /// 返回交易所报的累计成交量快照、是否有订单结果不明、以及这次从订单状态里
    /// 补记了几笔成交。快照供仓位核对使用：成交记录可能比挂单状态晚到，
    /// 这段差额是能解释的（第 7.1.3 节）。
    ///
    /// 一张单完全成交后同样会消失，所以不能只凭「过了 `expiresAfter`」判它撤销，
    /// 要问交易所它是什么结局（第 9.2 节）。
    ///
    /// 核对范围是「还在场」加「成交还没核对完整」的订单：终态化不等于成交已经对完，
    /// 撤销前可能已经有成交、成交记录也可能还没到。没对完的订单要继续查，
    /// 它们也是成交回查起点的下限（见 [`Runtime::fills_since`]）。
    async fn reconcile_orders(
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
        let mut unknown_result = false;
        let mut applied_fills = 0;

        for order in orders
            .iter_mut()
            .filter(|order| order.record.is_working() || !order.fills_reconciled)
        {
            let (exchange_view, observed_at) = match exchange_working
                .iter()
                .find(|working| working.client_order_id == order.record.client_order_id)
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
            let facts = ExchangeFacts {
                view: &exchange_view,
                saved_fills: saved_fills
                    .get(&order.record.client_order_id)
                    .copied()
                    .unwrap_or(Quantity::ZERO),
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
            unknown_result,
            applied_fills,
            inconsistent: inconsistent_fill_records(&saved_fills, &order_states(orders)),
        })
    }

    /// 按交易所给的订单状态收尾一张订单：更新本地记录，必要时把交易所认到的成交量
    /// 记入格子。返回是否新记了一笔成交。
    async fn settle_order(
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
        let saved = facts.saved_fills;

        // 交易所还认它是挂单：按挂单状态更新。成交还没对完，回查起点继续保留，
        // 规划也不为这个单位下同一用途的新单。
        if exchange_view.status.is_working() {
            let settled = fills_settled(
                order.record.cumulative_filled,
                exchange_view.cumulative_filled,
                saved,
            );
            let changed = Self::adopt_snapshot(order, exchange_view, now);
            if order.fills_reconciled != settled {
                order.fills_reconciled = settled;
                order.updated_at = now;
                self.store.update_order(strategy_id, order)?;
            } else if changed {
                self.store.update_order(strategy_id, order)?;
            }
            return Ok(false);
        }

        // 已成交，但成交记录还没到齐：保留预留等它补齐，不释放剩余量、也不重发。
        // 核对没完成，回查起点继续保留。（第 9.2.3 节的三项核对属于后续任务。）
        if exchange_view.status == OrderStatus::Filled && !unbooked.is_zero() {
            return Ok(false);
        }

        // 终态：把交易所认到的成交量吸收进格子。
        //
        // 挂单状态里没有成交时间，只能用拿到这份状态的时刻当上界：成交发生在查询之前，
        // 所以接收时刻一定不早于真实成交时刻。这一步开始的时刻不行——它可能早于真实
        // 成交时刻（查询发生在这一步中途），记下来之后真实流水也改不回去
        // （[`Strategy::record_fill_time`] 只往早里改）。
        let booked = !unbooked.is_zero()
            && !strategy
                .apply_fill(
                    state,
                    &mut order.record,
                    exchange_view.cumulative_filled,
                    facts.observed_at,
                )
                .is_duplicate();
        order.exchange_order_id = Some(exchange_view.exchange_order_id.clone());
        order.record.status = exchange_view.status;
        // 交易所的结局已知、记入的累计量也追上了，但成交流水还可能没到：流水表里
        // 属于这张单的数量之和没追平时不算对完账，回查起点要继续保留，
        // 否则这段流水会被永久漏掉。
        order.fills_reconciled = fills_settled(
            order.record.cumulative_filled,
            exchange_view.cumulative_filled,
            saved,
        );
        order.updated_at = now;
        if booked {
            // 格子状态和订单记录写在同一个事务里。
            self.store
                .save_applied_fills(strategy_id, state, &[&*order])?;
        } else {
            self.store.update_order(strategy_id, order)?;
        }
        Ok(booked)
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

    /// 拉成交的起点。
    ///
    /// 从上次同步点往前多查一段，免得时间戳早于同步点的成交被永久漏掉；如果还有
    /// 订单的成交没核对完，就从最早的那张订单挂出去的时间查起，保证它的成交记录是
    /// 完整的——判定一张查不到的单「没有任何影响」时要靠这个完整性（第 9.2 节）。
    ///
    /// 这里不能只看在场订单：撤单会把订单变成终态，而撤销前的成交可能还没到；
    /// 只看在场订单就会让回查起点跟着同步点前移，把这段成交永久漏掉。
    fn fills_since(&self, stored: &StoredStrategy, orders: &[StoredOrder]) -> DateTime<Utc> {
        let watermark = stored.fills_synced_through - self.options.fills_lookback;
        orders
            .iter()
            .filter(|order| order.record.is_working() || !order.fills_reconciled)
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
            // 记入了新的成交，这张单原来的「已对账」就不再成立：交易所的累计量刚往前走，
            // 对应的流水很可能还没到齐。由订单核对按新状态重新判定，不要沿用旧结论——
            // 沿用会让这张单从此不再被核对，回查起点也跟着同步点前移，
            // 还没到的流水就永久漏掉了。
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
                // 撤销只说明订单不再在场：撤销前可能已经有成交、成交记录也可能还没到，
                // 交易所给的最终累计成交量还不知道。核对没有完成，回查起点要继续保留，
                // 规划也要等对完账再按新的状态重算数量（第 9.2 节）。
                order.fills_reconciled = false;
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
        let state = self.store.load_state(strategy_id)?;
        strategy.validate_state(&state)?;
        Ok(state)
    }

    /// 按主仓成交推进批次（第 4.2 节）、按限速放行下一个单位（第 4.1 节），有变化就落盘。
    ///
    /// 记账之后、规划之前调用：新启用的格子和新放行的单位这一步就会挂上单，
    /// 提前结束的批次这一步就不再补挂主仓买单。
    ///
    /// 放行时刻由时钟给出，判断依据是记下来的真实成交时刻：重启和暂停都不会多给额度
    /// （第 9.4 节）。
    fn advance_progress(
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

/// 除限速间隔之外，新配置改动了的第一个字段。
///
/// 这些参数决定了批次划分和格子划分，运行中改动会让已保存的状态对不上，只能在
/// 停止后重建（第 2.4 节）。返回字段名是为了让报错说清楚改了什么。
///
/// 标的没有列在这里：新配置的标的必须和交易所的一致，规整时就会以配置错误拒绝。
fn changed_field(old: &StrategyConfig, new: &StrategyConfig) -> Option<&'static str> {
    // 先逐项比较，好报出具体是哪个字段被改了。
    let (old_side, new_side) = (&old.long, &new.long);
    if old_side.price_low != new_side.price_low {
        return Some("price_low");
    }
    if old_side.price_high != new_side.price_high {
        return Some("price_high");
    }
    if old_side.max_position != new_side.max_position {
        return Some("max_position");
    }
    if old_side.core_ratio != new_side.core_ratio {
        return Some("core_ratio");
    }
    if old_side.batch_count != new_side.batch_count {
        return Some("batch_count");
    }
    if old_side.grid_spacing != new_side.grid_spacing {
        return Some("grid_spacing");
    }
    if old_side.grid_spread != new_side.grid_spread {
        return Some("grid_spread");
    }
    // 兜底：以后给配置加了新参数而忘了在这里列出来，也必须是「拒绝」而不是「悄悄
    // 生效」——放行一个决定批次与格子划分的参数会让状态和配置对不上。比较的是整个
    // 配置，所以将来加在顶层的参数（比如空头侧）也挡得住。
    let mut unlisted = new.clone();
    unlisted.long.throttle_interval = old.long.throttle_interval;
    if unlisted != *old {
        return Some("unlisted");
    }
    None
}

/// 一张还没发出去的订单。
///
/// `expiresAfter` 按这张单实际发出去的时刻算，不是这一步开始的时刻：一步里可能挂好几张
/// 单，套用步首时间会让后面的单少活一段时间（第 9.1 节）。
fn new_order(intent: &OrderIntent, sent_at: DateTime<Utc>, expires_after: Duration) -> StoredOrder {
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

/// 判定需要的订单视图：只带判定用得到的字段。
fn order_states(orders: &[StoredOrder]) -> Vec<OrderState> {
    orders
        .iter()
        .map(|order| OrderState::of(&order.record, order.fills_reconciled))
        .collect()
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
fn drop_blocked_places(plan: &mut Plan, orders: &[StoredOrder]) {
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
fn refreshed_expiry(
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
    expires_after: Duration,
) -> Option<DateTime<Utc>> {
    let fresh = now + expires_after;
    (fresh > expires_at).then_some(fresh)
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
