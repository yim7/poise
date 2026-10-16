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
use poise_strategy::preview::{BatchSummary, CellSummary, ImmediateFill};
use poise_strategy::side::Direction;
use poise_strategy::state::{StateError, StrategyState};
use poise_strategy::units::{Notional, Position, Price, Quantity};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};

use crate::account::Account;
use crate::clock::{Clock, SystemClock};
use crate::exchange::{Exchange, ExchangeError, ExchangeOrder, OrderLookup, OrderRequest};
use crate::store::{
    DraftId, Store, StoreError, StoredConfigVersion, StoredDraft, StoredFill, StoredOrder,
    StoredStrategy, StrategyId, StrategyStatus, ViewSnapshot,
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
    /// 预览过期、或者预览之后仓位、可用抵押和会立即成交的订单变了：
    /// 旧预览说明不了现在会发生什么（B43）。
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
    /// 账户的可用抵押。和标记价一样在读取阶段读：视图快照要显示它，而读取之后
    /// 这一步已经可能下过单、撤过单，那时候再读失败，就等于把做成的一步报成失败。
    available_collateral: Notional,
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

/// 订单相关的读取结果：补记成交、核对挂单、推进批次与限速。
///
/// 判定和撤单都靠这三样算出的在场订单，所以它们失败时这一步就到此为止。
struct Synced {
    /// 补拉成交的回查起点。仓位核对的循环里还要用它再拉一次。
    since: DateTime<Utc>,
    /// 交易所报的累计成交量快照。
    reported: Vec<(ClientOrderId, Quantity)>,
    /// 是否有订单的结果不明。
    unknown_result: bool,
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
    available_collateral: Notional,
    /// 仓位核对的数据与结论。
    reading: PositionReading,
    check: PositionCheck,
    /// 核对循环里补记的成交笔数。
    applied_fills: usize,
    /// 核对循环里新发现的「订单结果不明」。
    unknown_result: bool,
    /// 核对循环里新发现的「成交流水与订单累计成交量对不上」。
    inconsistent: Option<ClientOrderId>,
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

    /// 新建草稿：只保存参数，不下任何单（第 2.4 节）。
    ///
    /// 参数这时不校验：合约规则和费率要问交易所，用户还没点预览。校验发生在预览和
    /// 确认启动时，用的是当时读到的规则。
    pub fn create_draft(&self, config: StrategyConfig) -> Result<DraftId, RuntimeError> {
        let now = self.clock.now();
        let draft_id = DraftId::new();
        self.store.insert_draft(&StoredDraft {
            draft_id: draft_id.clone(),
            config,
            created_at: now,
            updated_at: now,
        })?;
        tracing::info!(draft = %draft_id, "draft created");
        Ok(draft_id)
    }

    /// 保存草稿改过的参数，记一个新的参数版本，返回版本号（B42）。
    pub fn save_draft(
        &self,
        draft_id: &DraftId,
        config: StrategyConfig,
    ) -> Result<i64, RuntimeError> {
        let version = self
            .store
            .save_draft_config(draft_id, &config, self.clock.now())?;
        tracing::info!(draft = %draft_id, version, "draft saved");
        Ok(version)
    }

    pub fn draft(&self, draft_id: &DraftId) -> Result<StoredDraft, RuntimeError> {
        Ok(self.store.load_draft(draft_id)?)
    }

    pub fn drafts(&self) -> Result<Vec<StoredDraft>, RuntimeError> {
        Ok(self
            .store
            .load_draft_ids()?
            .iter()
            .map(|draft_id| self.store.load_draft(draft_id))
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// 删掉一个草稿。已经删掉的再删一次不报错。
    pub fn delete_draft(&self, draft_id: &DraftId) -> Result<(), RuntimeError> {
        self.store.delete_draft(draft_id)?;
        tracing::info!(draft = %draft_id, "draft deleted");
        Ok(())
    }

    /// 草稿预览（第 2.4 节）：校验结果、批次表、格子表、立即成交的订单和确认令牌。
    ///
    /// 不落库、不下单。合约规则和费率按现在问到的值算；校验不过时只有原因，
    /// 没有表格也没有令牌。
    ///
    /// 令牌带上预览时刻，确认启动时按它判断这份预览有多旧（[`PREVIEW_TTL`]）。
    pub async fn preview_draft(&self, draft_id: &DraftId) -> Result<DraftPreview, RuntimeError> {
        let draft = self.store.load_draft(draft_id)?;
        let instrument = draft.config.instrument.clone();
        let spec = self.account.instrument_spec(&instrument).await?;
        let fees = self.account.fees().await?;
        let market_price = self.exchange.mark_price(&instrument).await?;
        let position = self.exchange.position(&instrument).await?;
        let available_collateral = self.account.available_collateral().await?;
        let now = self.clock.now();

        let version = self.store.latest_config_version(draft_id)?;
        let mut preview = DraftPreview {
            draft_id: draft_id.clone(),
            instrument,
            config_version: version,
            validation: Validation {
                valid: true,
                reason: None,
            },
            market_price,
            position,
            available_collateral,
            batches: Vec::new(),
            cells: Vec::new(),
            immediate_fills: Vec::new(),
            token: None,
        };

        let strategy = match Strategy::new(draft.config, spec, fees) {
            Ok(strategy) => strategy,
            Err(error) => {
                preview.validation = Validation {
                    valid: false,
                    reason: Some(error.to_string()),
                };
                return Ok(preview);
            }
        };
        let state = strategy.initial_state();
        preview.batches = strategy.batch_summaries(&state);
        preview.cells = strategy.cell_summaries(&state, &[]);
        preview.immediate_fills = strategy.immediate_fills(market_price);
        preview.token = Some(preview_token(
            &preview,
            strategy.config(),
            strategy.spec(),
            strategy.fees(),
            now,
        )?);
        Ok(preview)
    }

    /// 确认启动：校验令牌、检查同一标的是否已有策略，然后写入策略记录（B42、B43、B46）。
    ///
    /// 令牌对不上说明预览之后仓位、可用抵押或会立即成交的订单变了，或者这份预览已经
    /// 超过 [`PREVIEW_TTL`]：旧预览说明不了现在会发生什么，要重新预览（B43）。
    /// 同一标的已有运行中或暂停的策略时拒绝启动（B46）。
    pub async fn start_draft(
        &self,
        draft_id: &DraftId,
        token: &str,
    ) -> Result<StrategyId, RuntimeError> {
        // 草稿标识和策略标识共用一套，所以这里也用它互斥：同一个草稿并发确认两次，
        // 只有一次能读到草稿并把它换成策略。
        let _guard = self.begin(draft_id)?;
        let now = self.clock.now();
        let draft = self.store.load_draft(draft_id)?;
        let instrument = draft.config.instrument.clone();
        let spec = self.account.instrument_spec(&instrument).await?;
        let fees = self.account.fees().await?;
        let strategy = Strategy::new(draft.config, spec, fees)?;

        let (previewed_at, fingerprint) = parse_token(token).ok_or(RuntimeError::PreviewStale(
            "the confirm token is not readable",
        ))?;
        let fresh = self.preview_draft(draft_id).await?;
        let fresh_fingerprint =
            preview_fingerprint(&fresh, strategy.config(), strategy.spec(), strategy.fees())?;
        if fresh.token.is_none() {
            return Err(RuntimeError::PreviewStale(
                "the config does not validate any more",
            ));
        }
        if fresh_fingerprint != fingerprint {
            return Err(RuntimeError::PreviewStale(
                "the position, the available collateral or the orders that would fill immediately changed",
            ));
        }
        // 时长落在未来（时钟被回拨，或者令牌里的时刻是编的）也算过期：只判「大于 60 秒」
        // 的话，负的时长永远不大于它，60 秒这条上界就被绕过去了。
        let age = now.signed_duration_since(previewed_at);
        if age > PREVIEW_TTL || age < Duration::zero() {
            return Err(RuntimeError::PreviewStale(
                "the preview is older than 60 seconds",
            ));
        }

        let initial_state = strategy.initial_state();
        self.store
            .start_draft(
                draft_id,
                &StoredStrategy {
                    strategy_id: draft_id.clone(),
                    instrument,
                    status: StrategyStatus::Running,
                    config: strategy.config().clone(),
                    spec: strategy.spec().clone(),
                    fees: *strategy.fees(),
                    fills_synced_through: now,
                    created_at: now,
                    updated_at: now,
                },
                &initial_state,
                // 第一份视图快照就是确认启动这一刻读到的数：列表和详情从策略存在的那
                // 一刻起就有东西可显示，不必等第一步走完。
                &ViewSnapshot {
                    taken_at: now,
                    status: StrategyStatus::Running,
                    note: None,
                    market_price: fresh.market_price,
                    position: fresh.position,
                    expected_position: strategy.expected_position(&initial_state),
                    available_collateral: fresh.available_collateral,
                    waiting: Vec::new(),
                },
            )
            .map_err(|error| match error {
                // 同一个标的上已经有活动策略。检查在写入的那个事务里做，所以两个草稿
                // 同时确认也只有一个能建成（B46）。
                StoreError::InstrumentBusy {
                    instrument,
                    strategy_id,
                } => RuntimeError::InstrumentBusy {
                    instrument,
                    strategy_id,
                },
                other => RuntimeError::Store(other),
            })?;
        tracing::info!(
            strategy = %draft_id,
            instrument = strategy.spec().instrument().symbol(),
            sides = strategy.sides().len(),
            "strategy started from draft"
        );
        Ok(draft_id.clone())
    }

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
    async fn drive_inner(
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

    /// 运行中修改参数：目前只有限速间隔可以改（第 2.4 节）。
    ///
    /// 新配置按策略的标的与费率规整后，除了限速间隔必须和现有配置完全一致：改价格
    /// 范围、比例、批数或网格间距都会让已有的批次划分和格子状态对不上，必须停下来
    /// 重建。改动立刻生效——下一步读的就是新配置——并记一个新的参数版本。
    ///
    /// 加上限速也是改同一个参数：只重建「从不限速变成限速」的那几侧的放行进度，限速
    /// 从这一刻开始计时。已经开始的单位保持放行，还没开始的单位回到未放行——它们在场
    /// 上的订单留在原地，等重新放行之后再由规划处理（第 4.1 节）。另一侧没被改过，
    /// 放行进度和计时完全不动：重建会把它退回「最后一个已经开始的单位」并重新计时，
    /// 白白推后它下一次放行。
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
        self.refuse_archived(strategy_id)?;
        let stored = self.store.load_strategy(strategy_id)?;
        // 只有运行中的策略能改参数：暂停中/停止中在撤单收尾，恢复中还没恢复成功，
        // 改了也不会生效，只会让用户以为改动被接受了。
        if stored.status != StrategyStatus::Running {
            return Err(RuntimeError::NotRunning(strategy_id.clone(), stored.status));
        }
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
        let throttled: Vec<Direction> = Direction::ALL
            .into_iter()
            .filter(|direction| gained_throttle(&stored.config, &new_config, *direction))
            .collect();
        // 限速进度不属于配置，加上限速时要单独重建：按修改这一刻重建放行进度。它和新
        // 配置写在同一笔里——分成两次写，中间崩掉就留下「配置已经限速、进度还是全部
        // 放行」，重启后限速静默失效（第 4.1 节）。
        let state = if throttled.is_empty() {
            None
        } else {
            let mut state = self.load_state(strategy_id, &candidate)?;
            candidate.rebuild_throttle(&mut state, now, &throttled);
            Some(state)
        };
        let version = self
            .store
            .save_config(strategy_id, &new_config, state.as_ref(), now)?;
        tracing::info!(
            strategy = %strategy_id,
            version,
            long_throttle_interval = ?new_config.long.as_ref().and_then(|side| side.throttle_interval),
            short_throttle_interval = ?new_config.short.as_ref().and_then(|side| side.throttle_interval),
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

    /// 策略列表，新建的在前（第 11 节）。
    ///
    /// 只读库：仓位来自后台每一步之后落的视图快照（第 11 节）。不去问交易所——网页
    /// 每隔几秒刷新一次，每次都问一遍既慢，又要和常驻服务抢同一把驱动锁。
    pub fn strategies(&self) -> Result<Vec<StrategySummary>, RuntimeError> {
        let mut summaries = Vec::new();
        // `load_all_strategy_ids` 已经是新建的在前。
        for strategy_id in self.store.load_all_strategy_ids()? {
            summaries.push(self.summary(&strategy_id)?);
        }
        Ok(summaries)
    }

    /// 运行详情（第 11 节）：只读库，不下单、不撤单，也不问交易所。
    ///
    /// 页面上的行情、仓位、可用资金和等待原因都来自后台最近一次驱动落的视图快照，
    /// 快照带上取数时刻，页面能看出这份读数有多旧。页面刷得再勤也不会多一次交易所调用，
    /// 也不会挡住正在走的那一步。
    pub fn detail(&self, strategy_id: &StrategyId) -> Result<StrategyDetail, RuntimeError> {
        let summary = self.summary(strategy_id)?;
        let stored = self.store.load_strategy(strategy_id)?;
        let view = self.store.load_view(strategy_id)?;
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let state = self.load_state(strategy_id, &strategy)?;
        let orders = self.store.load_orders(strategy_id)?;
        let working: Vec<WorkingOrder> = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.record.working_view())
            .collect();
        Ok(StrategyDetail {
            summary,
            taken_at: view.taken_at,
            market_price: view.market_price,
            available_collateral: view.available_collateral,
            position: view.position,
            expected_position: view.expected_position,
            note: view.note,
            batches: strategy.batch_summaries(&state),
            cells: strategy.cell_summaries(&state, &working),
            orders: orders
                .into_iter()
                .filter(|order| order.record.is_working())
                .collect(),
            waiting: view.waiting,
            config_versions: self.store.load_config_versions(strategy_id)?,
        })
    }

    /// 列表和详情共用的那一行。
    ///
    /// 仓位取视图快照：它和详情里那份读数是同一次驱动读回来的，页面上的列表和详情
    /// 不会各说各话。在场订单数取订单表——那是本地的记录，比快照新。
    fn summary(&self, strategy_id: &StrategyId) -> Result<StrategySummary, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        let view = self.store.load_view(strategy_id)?;
        let orders = self.store.load_orders(strategy_id)?;
        Ok(StrategySummary {
            strategy_id: strategy_id.clone(),
            instrument: stored.instrument.clone(),
            status: stored.status,
            config_version: self.store.latest_config_version(strategy_id)?,
            created_at: stored.created_at,
            updated_at: stored.updated_at,
            position: view.position,
            working_orders: orders
                .iter()
                .filter(|order| order.record.is_working())
                .count(),
            config: stored.config,
        })
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
    async fn running_step(
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
            .stop_if_blocked(
                strategy_id,
                &strategy,
                &mut state,
                &mut orders,
                &snapshot,
                &decision,
                now,
            )
            .await?
        {
            self.save_view(strategy_id, &report, snapshot.available_collateral, now)
                .await?;
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

        let report = StepReport {
            strategy_id: strategy_id.clone(),
            status: StrategyStatus::Running,
            plan,
            placed: executed.placed,
            canceled: executed.canceled,
            applied_fills: snapshot.applied_fills,
            position: snapshot.reading.actual,
            expected_position: snapshot.reading.expected,
        };
        self.save_view(strategy_id, &report, snapshot.available_collateral, now)
            .await?;
        Ok(report)
    }

    /// 判定说要停下来时，收尾并返回报告；可以继续下单时返回 `None`。
    ///
    /// 等待只是「这一步的数据说明不了问题」，状态不变、下一步再核对；暂停是「继续自动
    /// 重试没有意义」，撤销全部挂单交给用户处理（第 9.2 节）。
    #[allow(clippy::too_many_arguments)]
    async fn stop_if_blocked(
        &self,
        strategy_id: &StrategyId,
        strategy: &Strategy,
        state: &mut StrategyState,
        orders: &mut Vec<StoredOrder>,
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
                let (status, round) = self
                    .pause_inner(strategy_id, strategy, state, orders, now)
                    .await?;
                let mut report =
                    snapshot.stop(strategy_id, status, reason.wait_reason(), round.canceled);
                report.applied_fills += round.applied_fills;
                Ok(Some(report))
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
        let synced = self
            .sync_step(strategy_id, stored, strategy, state, orders, now)
            .await?;
        let reading = self
            .reading_step(strategy_id, strategy, state, orders, &synced, now)
            .await?;
        Ok(Snapshot {
            market_price: reading.market_price,
            available_collateral: reading.available_collateral,
            reading: reading.reading,
            check: reading.check,
            unknown_result: synced.unknown_result || reading.unknown_result,
            inconsistent: synced.inconsistent.clone().or(reading.inconsistent),
            applied_fills: synced.applied_fills + reading.applied_fills,
        })
    }

    /// 订单相关的读取：补记成交、核对挂单、推进批次与限速（[`Runtime::read_step`] 的前半段）。
    ///
    /// 判定和撤单都靠这三样算出的在场订单：撤单要么撤错、要么漏撤，所以它们失败时这一步
    /// 不能继续。行情、可用资金、仓位是另一回事，见 [`Runtime::reading_step`]。
    async fn sync_step(
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
            unknown_result: reconciled.unknown_result,
            inconsistent: reconciled.inconsistent,
            applied_fills,
        })
    }

    /// 展示用的读数：标记价、可用资金、仓位核对（[`Runtime::read_step`] 的后半段）。
    ///
    /// 这些读数只进视图快照和仓位核对，不影响订单本身：拿不到时调用方保留上一份快照
    /// 并说明原因，而不是把已经做成的一步报成失败。
    async fn reading_step(
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
        let market_price = self.exchange.mark_price(instrument).await?;
        let available_collateral = self.account.available_collateral().await?;

        let since = synced.since;
        let mut reported = synced.reported.clone();
        let mut applied_fills = 0;
        let mut unknown_result = false;
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
        Ok(Reading {
            market_price,
            available_collateral,
            reading,
            check,
            applied_fills,
            unknown_result,
            inconsistent,
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
    async fn pause_inner(
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
    async fn cancel_only_step_by_id(
        &self,
        strategy_id: &StrategyId,
        now: DateTime<Utc>,
    ) -> Result<StepReport, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        self.cancel_only_step(strategy_id, &stored, now).await
    }

    async fn cancel_only_step(
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
    async fn cancel_only_round(
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
    async fn cancel_only(
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

    /// 恢复中的策略走一步：先把遗留订单收干净，收干净了就转成运行中接着跑。
    ///
    /// 调用方必须已经通过 [`Runtime::begin`] 取得这个策略的驱动权。
    ///
    /// 一轮只做能做完的那部分：遗留订单还没过期、还没撤掉就什么都不做，等下一轮。常驻循环
    /// 每几秒一轮，比在恢复里睡一觉更好——睡着的时候别的策略也驱动不了。
    async fn restore_step(
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
    fn nothing_happened(&self, strategy_id: &StrategyId, status: StrategyStatus) -> StepReport {
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
    async fn resume_inner(
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

    async fn resume_once(
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

    /// 归档的策略只读：写操作一律拒绝（第 2.4 节）。
    fn refuse_archived(&self, strategy_id: &StrategyId) -> Result<(), RuntimeError> {
        if self.store.load_strategy(strategy_id)?.status == StrategyStatus::Stopped {
            return Err(RuntimeError::Archived(strategy_id.clone()));
        }
        Ok(())
    }

    /// 落一份视图快照（第 11 节）：网页的列表与详情只读它，不再去问交易所。
    ///
    /// 调用方必须在订单、状态都写完盘之后调它——快照里的仓位和在场订单数是给用户看的
    /// 「这一刻的样子」，先落快照再改库就会自相矛盾。
    ///
    /// 读数全部来自这一步读取阶段的那份快照，这里不再问交易所：走到这里单已经下出去、
    /// 撤回来了，再补一次查询失败，就会把做成的一步报成失败，用户看到「暂停失败」而
    /// 单其实已经撤了。
    async fn save_view(
        &self,
        strategy_id: &StrategyId,
        report: &StepReport,
        available_collateral: Notional,
        now: DateTime<Utc>,
    ) -> Result<(), RuntimeError> {
        // 只撤单的状态还没走完，原因要写清楚：页面上只有状态名的话，用户看不出它是在
        // 重试撤单、还是在等成交对账，也不知道该不该等下去。
        let note = if report.status.is_cancel_only() {
            cancel_only_note(&self.store.load_orders(strategy_id)?, report.status)
        } else {
            None
        };
        let view = ViewSnapshot {
            taken_at: now,
            status: report.status,
            note,
            market_price: report.plan.market_price,
            position: report.position,
            expected_position: report.expected_position,
            available_collateral,
            waiting: report.plan.waiting.clone(),
        };
        self.store.save_view(strategy_id, &view)?;
        Ok(())
    }

    /// 只改状态和说明，读数沿用上一份快照。
    ///
    /// 恢复没走通、还在等订单过期时用：这一刻本来就没有新读数（读取这一步本身就没做成），
    /// 但网页要能看到它在恢复、卡在哪。
    ///
    /// `taken_at` 原样保留：它说的是「这份读数是什么时候取的」。改成现在，用户就会把一份
    /// 十分钟前的仓位当成刚取的——说明文字补不回丢掉的时间（第 11 节）。状态什么时候变的
    /// 另有一处：`strategies.updated_at` 由 [`Store::save_strategy_status`] 写。
    async fn annotate_view(
        &self,
        strategy_id: &StrategyId,
        status: StrategyStatus,
        note: String,
    ) -> Result<(), RuntimeError> {
        let mut view = self.store.load_view(strategy_id)?;
        view.status = status;
        view.note = Some(note);
        self.store.save_view(strategy_id, &view)?;
        Ok(())
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

/// 预览的有效期：超过这段时间就要重新预览（第 2.4 节）。
///
/// 预览说的是「现在会发生什么」：仓位、可用抵押和会立即成交的订单都可能在这段时间里
/// 变了。标记价本身不算——它每时每刻都在动，只看它等于永远要求重新预览；要看的是它
/// 算出来的结果，也就是会立即成交的那份名单。
const PREVIEW_TTL: Duration = Duration::seconds(60);

/// 确认令牌：这次预览说明的那些事实的指纹，加上预览的时刻。
///
/// 指纹覆盖参数、合约规则、费率、算出来的表格、仓位和可用抵押——它们里的任何一个变了，
/// 旧预览就不再说明现在会发生什么，确认启动会被要求重新预览（B43）。
///
/// 标记价本身不进指纹：它每时每刻都在动，进指纹就等于永远要求重新预览。要看的是它
/// 算出来的结果——哪些订单会立即成交。标记价变了但这份名单没变，预览仍然是准的。
///
/// 令牌里带上预览时刻，确认启动时按它判断预览有多旧（[`PREVIEW_TTL`]）。
fn preview_token(
    preview: &DraftPreview,
    config: &StrategyConfig,
    spec: &InstrumentSpec,
    fees: &Fees,
    previewed_at: DateTime<Utc>,
) -> Result<String, StoreError> {
    let fingerprint = preview_fingerprint(preview, config, spec, fees)?;
    Ok(format!("{}.{fingerprint}", previewed_at.timestamp_millis()))
}

/// 从令牌里取出预览时刻和指纹。
///
/// 令牌是网页原样传回来的，解析不出来就当过期处理：重新预览一次没有代价，
/// 拿一个看不懂的令牌去启动才是问题。
fn parse_token(token: &str) -> Option<(DateTime<Utc>, &str)> {
    let (millis, fingerprint) = token.split_once('.')?;
    let millis: i64 = millis.parse().ok()?;
    if fingerprint.is_empty() {
        return None;
    }
    Some((DateTime::from_timestamp_millis(millis)?, fingerprint))
}

/// 预览说明的那些事实的指纹。
fn preview_fingerprint(
    preview: &DraftPreview,
    config: &StrategyConfig,
    spec: &InstrumentSpec,
    fees: &Fees,
) -> Result<String, StoreError> {
    let parts = [
        preview.draft_id.as_str().to_string(),
        preview.config_version.to_string(),
        serde_json::to_string(config)?,
        serde_json::to_string(spec)?,
        serde_json::to_string(fees)?,
        serde_json::to_string(&preview.batches)?,
        serde_json::to_string(&preview.cells)?,
        serde_json::to_string(&preview.immediate_fills)?,
        preview.position.to_string(),
        preview.available_collateral.to_string(),
    ];
    let mut hasher = Sha3_256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 除限速间隔之外，新配置改动了的第一个字段。
///
/// 这些参数决定了批次划分和格子划分，运行中改动会让已保存的状态对不上，只能在
/// 停止后重建（第 2.4 节）。返回字段名是为了让报错说清楚改了什么；两侧都看，
/// 哪一侧先被改动就报哪一侧（先多头侧、后空头侧）。
///
/// 标的没有列在这里：新配置的标的必须和交易所的一致，规整时就会以配置错误拒绝。
fn changed_field(old: &StrategyConfig, new: &StrategyConfig) -> Option<&'static str> {
    for direction in Direction::ALL {
        let (old_side, new_side) = (old.side(direction), new.side(direction));
        if old_side.is_some() != new_side.is_some() {
            return Some("sides");
        }
        let (Some(old_side), Some(new_side)) = (old_side, new_side) else {
            continue;
        };
        // 先逐项比较，好报出具体是哪个字段被改了。
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
    }
    // 兜底：以后给配置加了新参数而忘了在这里列出来，也必须是「拒绝」而不是「悄悄
    // 生效」——放行一个决定批次与格子划分的参数会让状态和配置对不上。比较的是整个
    // 配置，所以将来加在顶层的参数也挡得住。
    if without_throttle(new) != without_throttle(old) {
        return Some("unlisted");
    }
    None
}

/// 去掉两侧的限速参数：限速随时可以改，不参与「配置有没有被改过」的比较。
fn without_throttle(config: &StrategyConfig) -> StrategyConfig {
    let mut cleared = config.clone();
    for side in [&mut cleared.long, &mut cleared.short]
        .into_iter()
        .flatten()
    {
        side.throttle_interval = None;
    }
    cleared
}

/// 这一侧是不是「从不限速变成限速」。
///
/// 只有这样的侧需要重建放行进度。本来就有间隔的侧只是换了间隔长度，放行进度和计时
/// 照旧；不限速的侧没有进度可言。已经限速的侧改成不限速也不需要重建：规划对不限速
/// 的侧本来就不看放行进度，下一次走步会自己把状态写成不限速的样子。
fn gained_throttle(previous: &StrategyConfig, next: &StrategyConfig, direction: Direction) -> bool {
    let interval = |config: &StrategyConfig| {
        config
            .side(direction)
            .and_then(|side| side.throttle_interval)
    };
    interval(previous).is_none() && interval(next).is_some()
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

/// 只撤单的一步是否已经收尾：没有在场订单，而且每张订单的成交都对完了账。
///
/// 只看「没有在场订单」是不够的：撤销只把订单变成终态，撤销前可能已经有成交、成交
/// 记录也可能还没到。这时候归档，那段成交就再也没人补记了（第 9.2 节）。
///
/// 代价是：成交流水始终不到齐时，策略会一直停在「暂停中」「停止中」。这正是要的——
/// 停在半路能被看见，静默归档会把差额藏起来。
fn cancel_only_settled(orders: &[StoredOrder]) -> bool {
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
fn leftovers_settled(orders: &[StoredOrder]) -> bool {
    orders.iter().all(|order| !order.record.is_working())
}

/// 这张订单的结果我们从来没拿到过。
///
/// 下单超时（`Unknown`）和写库之后、发送之前崩溃（`Submitting`）都是这一类：交易所可能
/// 压根没收到这张单，撤销只会得到「查无此单」。
fn outcome_unknown(status: OrderStatus) -> bool {
    matches!(status, OrderStatus::Unknown | OrderStatus::Submitting)
}

/// 恢复中为什么还没转成「运行中」，写给用户看的一句话。
///
/// 两种原因分开说：撤销没确认的要重试；结果从来没拿到过的，重试也问不出结局——交易所只会
/// 回答「查无此单」，得等三方核对（任务 08）才能放行。混在一起说「正在重试」，用户会以为
/// 等下去就能好。
fn settle_note(orders: &[StoredOrder]) -> String {
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
fn cancel_only_note(orders: &[StoredOrder], status: StrategyStatus) -> Option<String> {
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
fn stale_reading_note(what: &str, progress: Option<String>) -> String {
    let stale = format!("这一步没读到{what}，读数沿用上一份。");
    match progress {
        Some(progress) => format!("{stale}{progress}"),
        None => stale,
    }
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
