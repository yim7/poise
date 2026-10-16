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
//! 代码按职责分在几个子模块里，全部是 [`Runtime`] 的方法：
//! - `lifecycle`：草稿、预览、确认启动、运行中修改参数；
//! - `step`：运行中的一步；
//! - `cancel_only`：暂停中、停止中、恢复中这几个只撤单的状态；
//! - `reconcile`：补记成交、按 `cloid` 核对订单、撤单；
//! - `view`：网页读的列表、详情和视图快照。
//!
//! 几个子模块共用的内部类型（读取快照、对账结果等）放在本文件。
//!
//! 时间全部来自注入的时钟（[`crate::clock::Clock`]）：订单的 `expiresAfter` 按实际
//! 发送时刻算，限速按实际成交时刻算（第 4.1、9.1 节）。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use poise_strategy::Strategy;
use poise_strategy::config::StrategyConfig;
use poise_strategy::decision::{OrderState, Permits, PositionCheck, PositionReading};
use poise_strategy::error::ConfigError;
use poise_strategy::instrument::Instrument;
use poise_strategy::order::ClientOrderId;
use poise_strategy::planning::{OrderIntent, Plan, WaitReason};
use poise_strategy::preview::{BatchSummary, CellSummary, ImmediateFill};
use poise_strategy::state::{StateError, StrategyState};
use poise_strategy::takeover::TakeoverError;
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};
use serde::{Deserialize, Serialize};

use crate::account::Account;
use crate::clock::{Clock, SystemClock};
use crate::exchange::{Exchange, ExchangeError, ExchangeOrder};
use crate::settings::SettingsError;
use crate::store::{
    DraftId, Store, StoreError, StoredConfigVersion, StoredFill, StoredOrder, StrategyId,
    StrategyStatus,
};

mod cancel_only;
mod funds;
mod lifecycle;
mod reconcile;
mod step;
mod unknown;
mod view;

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
    /// 结果未知的订单在 `expiresAfter` 之后再等多久，才按三项核对判定它没有影响（第 9.2 节）。
    ///
    /// `expiresAfter` 只保证超过时间戳的请求会被拒绝；交易所的挂单和成交查询可能比撮合
    /// 晚一点才看得到，余量就是留给这段延迟的。具体长度需要在测试网确认。
    pub unknown_order_grace: Duration,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            order_expires_after: Duration::seconds(5),
            fills_lookback: Duration::seconds(60),
            unknown_order_grace: Duration::seconds(30),
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
    /// 预览过期、或者预览之后仓位、可用抵押和会立即成交的订单变了：
    /// 旧预览说明不了现在会发生什么（B43）。
    /// 账户风控设置不合理（第 7.4 节）。
    #[error("the account settings are invalid: {0}")]
    Settings(#[from] SettingsError),
    /// 已有仓位接管不了（第 8 节第 1 条）。
    #[error("the existing position cannot be taken over: {0}")]
    Takeover(#[from] TakeoverError),
    #[error("the preview is out of date: {0}; preview again")]
    PreviewStale(&'static str),
    /// 一个标的同一时间只能有一个运行中的策略（B46）。
    #[error("instrument `{instrument}` already has a live strategy `{strategy_id}`")]
    InstrumentBusy {
        instrument: Instrument,
        strategy_id: StrategyId,
    },
    /// 归档的策略只读：停止之后不能改参数，也不能再驱动（第 2.4 节）。
    #[error("strategy `{0}` is archived and read-only")]
    Archived(StrategyId),
}

/// 走一步的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// 这一步是不是把策略停下来了。
    ///
    /// 停下来分两步：先落「暂停中」不再下任何新单，撤单和成交对账都收尾之后才转
    /// 「已暂停」。对「有没有停下」这个问题，两者是同一个答案。
    pub fn is_paused(&self) -> bool {
        matches!(
            self.status,
            StrategyStatus::Paused | StrategyStatus::Pausing
        )
    }
}

/// 一次订单核对的结果。
struct ReconcileOutcome {
    /// 交易所报的累计成交量快照，供仓位核对使用。
    reported: Vec<(ClientOrderId, Quantity)>,
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
    /// 标记价是什么时候读到的：发新单前按它判断行情有没有过期（第 7.2 节）。
    priced_at: DateTime<Utc>,
    /// 账户的可用抵押。和标记价一样在读取阶段读：视图快照要显示它，而读取之后
    /// 这一步已经可能下过单、撤过单，那时候再读失败，就等于把做成的一步报成失败。
    available_collateral: Notional,
    /// 这一轮读到的仓位核对数据：交易所的实际仓位、我们推算的应有仓位，
    /// 以及每张订单交易所报的累计成交量快照。
    reading: PositionReading,
    /// 仓位核对的结论。读取阶段的循环里已经算过，判定直接用，不重算一遍。
    check: PositionCheck,
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

/// 订单相关的读取结果：补记成交、核对挂单、推进批次与限速。
///
/// 判定和撤单都靠这三样算出的在场订单，所以它们失败时这一步就到此为止。
struct Synced {
    /// 补拉成交的回查起点。仓位核对的循环里还要用它再拉一次。
    since: DateTime<Utc>,
    /// 交易所报的累计成交量快照。
    reported: Vec<(ClientOrderId, Quantity)>,
    /// 成交流水与订单累计成交量对不上的订单。
    inconsistent: Option<ClientOrderId>,
    /// 这一步补记的成交笔数。
    applied_fills: usize,
}

/// 展示用的读数：标记价、可用资金、仓位核对。
///
/// 这三样只进视图快照和仓位核对，不影响订单本身。拿不到它们时调用方按「这一步没拿到
/// 读数」处理，而不是把已经做成的事报成失败。
struct Reading {
    market_price: Price,
    priced_at: DateTime<Utc>,
    available_collateral: Notional,
    /// 仓位核对的数据与结论。
    reading: PositionReading,
    check: PositionCheck,
    /// 核对循环里补记的成交笔数。
    applied_fills: usize,
    /// 核对循环里新发现的「成交流水与订单累计成交量对不上」。
    inconsistent: Option<ClientOrderId>,
}

/// 执行阶段发新单的条件：判定给出的许可，加上行情的读取时刻（第 7.2 节）。
#[derive(Clone, Copy)]
struct NewOrders {
    permits: Permits,
    priced_at: DateTime<Utc>,
    /// 行情读回来之后多久算过期：账户风控设置里的行情过期时间。
    max_age: Duration,
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

/// 只撤单的一步里「撤销 + 按 `cloid` 核对终态」这一段的结果。
///
/// 撤销和核对各自的失败都记在这里，不往外抛：撤销请求已经发出去、单可能已经撤掉了，
/// 把整步报成失败只会让用户看到「暂停失败」而单其实撤了。失败由调用方用来判定这一步
/// 能不能宣告收场——核对没走通就一律不能（第 9.2 节）。
struct CancelOnly {
    canceled: Vec<ClientOrderId>,
    /// 撤单之后重新核对时补记的成交笔数。
    applied_fills: usize,
    /// 撤销这一段没走通的原因（交易所拒收、数据库写不进去）。
    cancel_error: Option<RuntimeError>,
    /// 按 `cloid` 核对终态这一段没走通的原因。
    reconcile_error: Option<RuntimeError>,
}

/// 只撤单的一轮（[`Runtime::cancel_only_round`]）的结果。
///
/// 三处调用各自用它决定收场之后进入哪个状态，判定口径不同（见
/// [`cancel_only_settled`] 与 [`leftovers_settled`]），所以这里不替它们下结论，
/// 只把「这一轮有没有没走通的地方」报出来。
struct CancelOnlyRound {
    canceled: Vec<ClientOrderId>,
    /// 这一轮一共补记了几笔成交：同步那一段的和撤完之后核对那一段的都算。
    applied_fills: usize,
    /// 同步（补记成交、核对订单）的结果。失败时带着原因：撤单照做，但这一轮不能算
    /// 收场，展示读数也取不成了。
    synced: Result<Synced, RuntimeError>,
    /// 撤销、按 `cloid` 核对终态这两段没走通的原因。
    cancel_error: Option<RuntimeError>,
}

impl CancelOnlyRound {
    /// 这一轮有没有没走通的地方。有的话，订单看起来再干净也不能宣告完成。
    fn is_clean(&self) -> bool {
        self.synced.is_ok() && self.cancel_error.is_none()
    }
}

/// 恢复流程的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeOutcome {
    /// 还有可能落地的订单，等到这个时间点之后再来。
    WaitingForExpiry { until: DateTime<Utc> },
    /// 遗留订单还没收场（撤销没确认、终态没核对出来）：状态留在「恢复中」，
    /// 后台下一轮继续撤、继续核对。
    WaitingForSettle {
        /// 还在场、撤销还没确认的订单张数。
        working: usize,
    },
    /// 已经撤销遗留挂单并走了一步；这一步也可能又把它暂停了。
    Resumed(StepReport),
    /// 这个策略一直在运行，没有停过：挂单还是对的，什么都没做（第 2.4 节）。
    AlreadyRunning,
}

/// 预览里的校验结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validation {
    pub valid: bool,
    /// 校验不过时的原因；通过时是 `None`。
    pub reason: Option<String>,
}

/// 草稿预览（第 2.4 节）。
///
/// 校验不过时只有原因：没有表格，也没有令牌，因此确认启动无从谈起。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftPreview {
    pub draft_id: DraftId,
    pub instrument: Instrument,
    /// 这次预览用的参数版本。
    pub config_version: i64,
    pub validation: Validation,
    /// 交易所当前的标记价。
    pub market_price: Price,
    /// 交易所上这个标的当前的仓位：标的已有仓位时，预览要能看出接管分配（第 8 节）。
    pub position: Position,
    pub available_collateral: Notional,
    pub batches: Vec<BatchSummary>,
    pub cells: Vec<CellSummary>,
    pub immediate_fills: Vec<ImmediateFill>,
    /// 启动后第一步会挂出的订单：限价已经按第 6 节避开本策略自己的反向订单。接管已有
    /// 仓位时，用户靠它确认持有格子的平仓单和没买完的主仓买单（第 8 节）。
    pub planned_orders: Vec<OrderIntent>,
    /// 交易所上这个标的已有的挂单。启动前它们都不属于本策略：本系统不撤销，由用户处理
    /// （第 8 节第 4 条）。
    pub foreign_orders: Vec<ExchangeOrder>,
    /// 确认启动时要原样带回来的令牌；校验不过时是 `None`。
    pub token: Option<String>,
}

/// 策略列表里的一行（第 11 节）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategySummary {
    pub strategy_id: StrategyId,
    pub instrument: Instrument,
    pub status: StrategyStatus,
    /// 当前生效的参数版本。
    pub config_version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// 交易所上这个标的的净仓位。
    pub position: Position,
    /// 在场挂单数量。
    pub working_orders: usize,
    /// 当前参数。列表上直接显示摘要，不必再取一次。
    pub config: StrategyConfig,
}

/// 运行详情（第 11 节）：批次表、格子表、在场挂单、等待原因、参数版本。
///
/// 全部来自本地：批次表、格子表、订单和参数版本是库里的记录，行情、仓位、可用资金和
/// 等待原因来自最近一份视图快照。`taken_at` 是那份快照的时刻，页面据此说明读数有多旧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyDetail {
    pub summary: StrategySummary,
    /// 行情、仓位这些读数是什么时候取的。
    pub taken_at: DateTime<Utc>,
    pub market_price: Price,
    pub available_collateral: Notional,
    /// 账户保证金率（第 7.4 节）；那一步没读到时是 `None`。
    pub margin_ratio: Option<Rate>,
    /// 交易所上的仓位与按状态算出的应有持仓；对不上时策略会被暂停。
    pub position: Position,
    pub expected_position: Position,
    /// 这个策略为什么还没走完当前状态；没有特殊情况时是 `None`。
    pub note: Option<String>,
    pub batches: Vec<BatchSummary>,
    pub cells: Vec<CellSummary>,
    /// 还在场的挂单。
    pub orders: Vec<StoredOrder>,
    /// 最近一步在等什么。运行中的策略才有；暂停和归档的策略不在下单，没有什么可等的。
    pub waiting: Vec<WaitReason>,
    pub config_versions: Vec<StoredConfigVersion>,
}

/// 等正在走的那一步时，每轮之间歇多久、最多等几轮（合起来约 3 秒）。
const WAIT_STEP_MS: u64 = 200;
const WAIT_ROUNDS: usize = 15;

pub struct Runtime {
    exchange: Arc<dyn Exchange>,
    /// 账户级查询：合约规则、费率、可用抵押、账户模式。
    account: Arc<dyn Account>,
    store: Store,
    options: RuntimeOptions,
    /// 当前时间的来源。策略是纯计算，时间只在这里取。
    clock: Arc<dyn Clock>,
    /// 正在被驱动的策略。同一个策略并发走两步会用同一份状态各下一遍单。
    stepping: Mutex<HashSet<StrategyId>>,
    /// 资金预检的锁和保证金不足的退避表（第 10 节）：同一账户的所有策略共用一份。
    funds: funds::Funds,
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
    pub fn new(exchange: Arc<dyn Exchange>, account: Arc<dyn Account>, store: Store) -> Self {
        Self::with_options(exchange, account, store, RuntimeOptions::default())
    }

    pub fn with_options(
        exchange: Arc<dyn Exchange>,
        account: Arc<dyn Account>,
        store: Store,
        options: RuntimeOptions,
    ) -> Self {
        Self::with_clock(exchange, account, store, options, Arc::new(SystemClock))
    }

    /// 用给定的时钟建运行时。测试注入可控时钟，时间就不再依赖跑得多快。
    pub fn with_clock(
        exchange: Arc<dyn Exchange>,
        account: Arc<dyn Account>,
        store: Store,
        options: RuntimeOptions,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            exchange,
            account,
            store,
            options,
            clock,
            stepping: Mutex::new(HashSet::new()),
            funds: funds::Funds::default(),
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

    /// 读取和用户操作等正在走的那一步结束，而不是直接报忙。
    ///
    /// 网页每隔几秒刷新一次详情，常驻服务每隔几秒走一步，两者撞上是常事。把「忙」
    /// 当成失败，页面就会周期性地闪一下错误、按钮点了没反应。一步是有头的，等一会儿
    /// 就好；真等不到（比如交易所卡住）才报忙。
    async fn begin_waiting(&self, strategy_id: &StrategyId) -> Result<StepGuard<'_>, RuntimeError> {
        for _ in 0..WAIT_ROUNDS {
            match self.begin(strategy_id) {
                Err(RuntimeError::Busy(_)) => {
                    tokio::time::sleep(std::time::Duration::from_millis(WAIT_STEP_MS)).await;
                }
                other => return other,
            }
        }
        self.begin(strategy_id)
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn options(&self) -> &RuntimeOptions {
        &self.options
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

    /// 按状态挑出策略，最近建的在前。
    fn with_status(&self, wanted: &[StrategyStatus]) -> Result<Vec<StrategyId>, RuntimeError> {
        let mut matched = Vec::new();
        for strategy_id in self.store.load_all_strategy_ids()? {
            let status = self.store.load_strategy(&strategy_id)?.status;
            if wanted.contains(&status) {
                matched.push(strategy_id);
            }
        }
        Ok(matched)
    }

    /// 归档的策略只读：写操作一律拒绝（第 2.4 节）。
    fn refuse_archived(&self, strategy_id: &StrategyId) -> Result<(), RuntimeError> {
        if self.store.load_strategy(strategy_id)?.status == StrategyStatus::Stopped {
            return Err(RuntimeError::Archived(strategy_id.clone()));
        }
        Ok(())
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
}

/// 判定需要的订单视图：只带判定用得到的字段。
fn order_states(orders: &[StoredOrder]) -> Vec<OrderState> {
    orders
        .iter()
        .map(|order| OrderState::of(&order.record, order.fills_reconciled))
        .collect()
}
