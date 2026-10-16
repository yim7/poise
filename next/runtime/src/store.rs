//! SQLite 持久化。
//!
//! 订单在发送给交易所之前先写进这里；策略、格子状态、成交记录也都在这里。
//! 数值一律按定点整数单位存，时间存成可比较的 UTC 字符串。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};
use poise_strategy::batch::BatchIndex;
use poise_strategy::config::StrategyConfig;
use poise_strategy::planning::WaitReason;

use poise_strategy::grid::CellIndex;
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Unit};
use poise_strategy::side::Direction;
use poise_strategy::state::{BatchState, CellState, SideState, StrategyState};
use poise_strategy::throttle::ThrottleState;
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};

use crate::settings::AccountSettings;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 策略标识。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StrategyId(String);

impl StrategyId {
    pub fn new() -> Self {
        Self(format!("s-{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for StrategyId {
    fn default() -> Self {
        Self::new()
    }
}

/// 草稿标识。
///
/// 和策略标识共用一套：草稿保存过的参数版本在确认启动后原样延续给策略，
/// 事后能看出这次启动用的是第几个版本（第 2.4 节）。
pub type DraftId = StrategyId;

impl std::fmt::Display for StrategyId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for StrategyId {
    type Error = StoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.trim().is_empty() {
            return Err(StoreError::EmptyStrategyId);
        }
        Ok(Self(value))
    }
}

/// 接口上的标识就是字符串；反序列化仍然走 [`StrategyId::try_from`]，空标识读不进来。
impl Serialize for StrategyId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for StrategyId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        StrategyId::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// 策略生命周期状态（第 2.4 节）。
///
/// 「暂停中」「停止中」是只撤单的过渡状态：暂停和停止都先落到这里，后台继续撤单、
/// 查订单终态、补记成交，全部订单结束且成交对完账之后才转成「已暂停」或「已归档」。
/// 先落状态再动手，是为了让「不再下新单」这件事在崩溃后依然成立——只存在内存里的
/// 意图，重启就没了。
///
/// 设计里还有「已失效」，它由风控触发，属于后续任务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyStatus {
    /// 草稿：参数已保存，还没有开始下单。草稿不在 `strategies` 表里。
    Draft,
    Running,
    /// 恢复中：进程重启后还没恢复成功，不下任何新单，后台继续重试。
    ///
    /// 恢复失败（交易所连不上、账户查不到）不能把策略放回「运行中」：那会让它在
    /// 遗留订单还没撤掉的情况下重新挂单。也不能直接暂停它——暂停会撤掉全部订单，
    /// 一次临时故障不该让用户的挂单全部消失。
    Restoring,
    /// 暂停中：不再下新单，正在撤掉在场订单、把成交对完账。
    Pausing,
    Paused,
    /// 停止中：不再下新单，撤干净、对完账之后归档。
    Stopping,
    /// 已停止：订单已撤销、仓位保留，策略归档只读。
    Stopped,
}

impl StrategyStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Running => "running",
            Self::Restoring => "restoring",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }

    /// 这个状态下策略还在交易所里活动（在场订单由它负责）。
    ///
    /// 「恢复中」「暂停中」「停止中」也算：订单还没撤干净，这个标的不能交给别的策略（B46）。
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            Self::Running | Self::Restoring | Self::Pausing | Self::Paused | Self::Stopping
        )
    }

    /// 只撤单：不下任何新单，只撤掉在场订单、按 `cloid` 查终态、补记成交。
    pub const fn is_cancel_only(self) -> bool {
        matches!(self, Self::Pausing | Self::Stopping)
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "draft" => Ok(Self::Draft),
            "running" => Ok(Self::Running),
            "restoring" => Ok(Self::Restoring),
            "pausing" => Ok(Self::Pausing),
            "paused" => Ok(Self::Paused),
            "stopping" => Ok(Self::Stopping),
            "stopped" => Ok(Self::Stopped),
            other => Err(StoreError::UnknownStatus {
                value: other.to_string(),
            }),
        }
    }
}

/// 接口上的状态就是数据库里那个字符串：两边说的是同一件事，不该有两套写法。
impl Serialize for StrategyStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StrategyStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        StrategyStatus::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// 一个策略的持久化记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredStrategy {
    pub strategy_id: StrategyId,
    pub instrument: Instrument,
    pub status: StrategyStatus,
    pub config: StrategyConfig,
    pub spec: InstrumentSpec,
    pub fees: Fees,
    /// 已经补记过的成交时间点，下次只取它之后的成交。
    pub fills_synced_through: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 一张订单的持久化记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredOrder {
    pub record: OrderRecord,
    pub exchange_order_id: Option<String>,
    /// 这张订单的成交是否已经核对完整。
    ///
    /// 终态和「成交核对完整」是两件事：撤单成功只说明它不再在场，撤销前可能已经有成交，
    /// 成交记录也可能还没到。核对没完成的订单要继续拉成交，所以这个标记和状态分开存。
    pub fills_reconciled: bool,
    /// 超过这个时间点，交易所不会再接受这张订单。
    pub expires_at: DateTime<Utc>,
    pub placed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 一个草稿的持久化记录（第 2.4 节）。
///
/// 草稿只有参数，没有批次与格子状态：它还没开始下单，划分要在预览和确认启动时
/// 按当时的合约规则和费率重新算一遍。标识和策略共用一套：确认启动后参数版本原样
/// 延续，事后能看出这次启动用的是第几个版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDraft {
    pub draft_id: StrategyId,
    pub config: StrategyConfig,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 一次参数的存档：运行中改了可以改的参数就记一条（第 2.4 节）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredConfigVersion {
    /// 从 1 开始，按记录顺序递增。
    pub version: i64,
    pub config: StrategyConfig,
    pub recorded_at: DateTime<Utc>,
}

/// 一笔已补记的成交。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFill {
    pub trade_id: String,
    pub strategy_id: StrategyId,
    pub client_order_id: ClientOrderId,
    pub price: Price,
    pub quantity: Quantity,
    pub cumulative_quantity: Quantity,
    pub filled_at: DateTime<Utc>,
}

/// 一份视图快照：后台每走一步就落一份（第 11 节）。
///
/// 网页的列表和详情只读它。不这样做的话，每次刷新页面都要问一遍交易所的行情、仓位和
/// 可用资金，还要和常驻服务抢同一把驱动锁——页面慢、驱动也被拖住，而页面想要的其实
/// 只是「最近一次读到的数」。
///
/// 每个策略从建起来那一刻就有快照：确认启动时读到的仓位和可用资金就是第一份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSnapshot {
    /// 这份读数是什么时候取的。
    pub taken_at: DateTime<Utc>,
    /// 这份快照对应的策略状态。
    pub status: StrategyStatus,
    /// 一句话说明这个策略为什么还没走完当前状态；没有特殊情况时是 `None`。
    pub note: Option<String>,
    pub market_price: Price,
    /// 交易所上的仓位。
    pub position: Position,
    /// 按状态算出的应有持仓。
    pub expected_position: Position,
    pub available_collateral: Notional,
    /// 账户保证金率（第 7.4 节）；这一步没读到时是 `None`。
    #[serde(default)]
    pub margin_ratio: Option<Rate>,
    /// 最近一步在等什么。
    pub waiting: Vec<WaitReason>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("stored json is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("strategy id must not be empty")]
    EmptyStrategyId,
    #[error("strategy `{0}` is not stored")]
    UnknownStrategy(StrategyId),
    #[error("strategy `{0}` has no view snapshot")]
    UnknownView(StrategyId),
    #[error("draft `{0}` is not stored")]
    UnknownDraft(StrategyId),
    #[error(
        "instrument `{}` already has a live strategy `{strategy_id}`",
        instrument.symbol()
    )]
    InstrumentBusy {
        instrument: Instrument,
        strategy_id: StrategyId,
    },
    #[error("unknown strategy status `{value}`")]
    UnknownStatus { value: String },
    #[error("unknown order purpose `{value}`")]
    UnknownPurpose { value: String },
    #[error("unknown order unit `{value}`")]
    UnknownUnit { value: String },
    #[error("unknown direction `{value}`")]
    UnknownDirection { value: String },
    #[error("unknown order status `{value}`")]
    UnknownOrderStatus { value: String },
    #[error("write failure injected by the test hook")]
    InjectedWriteFailure,
    #[error("column `{column}` holds an invalid value `{value}`")]
    InvalidColumn { column: &'static str, value: String },
    #[error(
        "database `{location}` has schema version {found}, but this build needs version {expected}; \
         this project does not migrate old databases — move the old database away and restart"
    )]
    SchemaVersion {
        /// 库的位置，或者 `in-memory`。
        location: String,
        /// 库里记的结构版本。
        found: i32,
        /// 这个版本期望的结构版本。
        expected: i32,
    },
}

/// 数据库结构版本，记在 SQLite 的 `user_version` 里。
///
/// 改表结构就加一。打开库时版本对不上直接拒绝，而不是等写到一半才发现少了一列——
/// 那时候用户已经以为策略在跑了（第 12 节：不兼容旧数据库，也不迁移旧库）。
const SCHEMA_VERSION: i32 = 2;

/// 库里已经有表了吗。
///
/// 有表就说明这不是一个刚打开的空文件，而是别人建好的库：它没有版本记录，是因为它
/// 建于「记版本」这件事之前。
fn has_tables(connection: &Connection) -> Result<bool, rusqlite::Error> {
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// 一个进程内的数据库句柄。克隆出来的句柄共用同一条连接。
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    /// 还要让几次成交记账写库失败，见 [`Store::fail_applied_fills`]。
    injected_write_failures: Arc<AtomicUsize>,
    /// 下一次写入符合条件的订单时失败，见 [`Store::fail_next_order_update`]。
    injected_order_failure: Arc<Mutex<Option<OrderMatcher>>>,
}

/// 测试用：挑出要让写库失败的那一次订单写入。
type OrderMatcher = Box<dyn Fn(&StoredOrder) -> bool + Send + Sync>;

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let connection = Connection::open(path)?;
        Self::from_connection(connection, &path.display().to_string())
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?, "in-memory")
    }

    /// 建表（新库）或者核对结构版本（已有库），`source` 只用在错误信息里。
    fn from_connection(mut connection: Connection, source: &str) -> Result<Self, StoreError> {
        let found: i32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        // 0 是「没有记过版本」：全新的空库，或者这个版本之前建的旧库。前者正是接下来
        // 要建表的那个，后者要拒绝——旧库有没有这一版要的列，只有建表时才知道，
        // 而「表已经在了，`CREATE TABLE IF NOT EXISTS` 什么都不会做」。
        if found != SCHEMA_VERSION && (found != 0 || has_tables(&connection)?) {
            return Err(StoreError::SchemaVersion {
                location: source.to_string(),
                found,
                expected: SCHEMA_VERSION,
            });
        }
        // 这两条只能在事务外设：`journal_mode` 在事务里改不了，`foreign_keys` 在事务里
        // 是空操作。
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;",
        )?;
        // 建表和写版本在同一个事务里。分成两步的话，建表成功之后、版本写下去之前进程退出，
        // 会留下一个「版本 0、有表」的库：下一次打开被当成旧库拒绝，而它其实是这一版的库，
        // 只是没建完。放进事务，中途失败就一起回滚，重开还能从零建起来。
        let transaction = connection.transaction()?;
        transaction.execute_batch(TABLES)?;
        // 表建好了才写版本：写在前头、建表失败的话，这个库会被当成「已经是这一版」，
        // 下一次打开就少了一张表却查不出原因。
        transaction.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        transaction.commit()?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            injected_write_failures: Arc::new(AtomicUsize::new(0)),
            injected_order_failure: Arc::new(Mutex::new(None)),
        })
    }

    /// 让接下来 `count` 次成交记账写库失败（[`Store::save_applied_fills`]）。
    ///
    /// 只给测试用：磁盘满、库被别的进程锁住这类真实的写失败没法按需造出来，而「写失败
    /// 之后内存里那份改动还算不算数」正是要验的东西。失败点在事务之前，所以流水那一笔
    /// 已经写进去了——这正是要复现的中间态：流水在，格子和订单的累计成交量没跟上。
    pub fn fail_applied_fills(&self, count: usize) {
        self.injected_write_failures.store(count, Ordering::SeqCst);
    }

    /// 让下一次符合 `matching` 的订单写入（[`Store::update_order`]）失败，只失败一次。
    ///
    /// 只给测试用：和 [`Store::fail_applied_fills`] 一样，要验的是「写失败之后内存里那份
    /// 改动还算不算数」。按条件挑，是因为一步里会写很多次订单，按次数数不准。
    pub fn fail_next_order_update(
        &self,
        matching: impl Fn(&StoredOrder) -> bool + Send + Sync + 'static,
    ) {
        *self
            .injected_order_failure
            .lock()
            .expect("injection lock poisoned") = Some(Box::new(matching));
    }

    /// 取走一次注入的写失败。
    fn take_injected_write_failure(&self) -> bool {
        self.injected_write_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                (left > 0).then(|| left - 1)
            })
            .is_ok()
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().expect("store lock poisoned")
    }
}

/// 这一版的建表语句，整体跑在一个事务里（见 [`Store::from_connection`]）。
const TABLES: &str = "
             CREATE TABLE IF NOT EXISTS strategies (
                 strategy_id TEXT PRIMARY KEY,
                 instrument TEXT NOT NULL,
                 status TEXT NOT NULL,
                 config_json TEXT NOT NULL,
                 spec_json TEXT NOT NULL,
                 fees_json TEXT NOT NULL,
                 fills_synced_through TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS drafts (
                 draft_id TEXT PRIMARY KEY,
                 config_json TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS batch_states (
                 strategy_id TEXT NOT NULL,
                 direction TEXT NOT NULL,
                 batch_index INTEGER NOT NULL,
                 filled_units INTEGER NOT NULL,
                 finished INTEGER NOT NULL,
                 PRIMARY KEY (strategy_id, direction, batch_index)
             );
             CREATE TABLE IF NOT EXISTS cell_states (
                 strategy_id TEXT NOT NULL,
                 direction TEXT NOT NULL,
                 cell_index INTEGER NOT NULL,
                 enabled INTEGER NOT NULL,
                 to_buy_back_units INTEGER NOT NULL,
                 PRIMARY KEY (strategy_id, direction, cell_index)
             );
             CREATE TABLE IF NOT EXISTS orders (
                 client_order_id TEXT PRIMARY KEY,
                 strategy_id TEXT NOT NULL,
                 unit_kind TEXT NOT NULL,
                 unit_direction TEXT NOT NULL,
                 unit_index INTEGER NOT NULL,
                 purpose TEXT NOT NULL,
                 limit_price_units INTEGER NOT NULL,
                 quantity_units INTEGER NOT NULL,
                 cumulative_filled_units INTEGER NOT NULL,
                 reduce_only INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 fills_reconciled INTEGER NOT NULL,
                 exchange_order_id TEXT,
                 expires_at TEXT NOT NULL,
                 placed_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS orders_by_strategy
                 ON orders (strategy_id, placed_at);
             CREATE TABLE IF NOT EXISTS fills (
                 trade_id TEXT PRIMARY KEY,
                 strategy_id TEXT NOT NULL,
                 client_order_id TEXT NOT NULL,
                 price_units INTEGER NOT NULL,
                 quantity_units INTEGER NOT NULL,
                 cumulative_quantity_units INTEGER NOT NULL,
                 filled_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS fills_by_strategy ON fills (strategy_id, filled_at);
             CREATE TABLE IF NOT EXISTS throttle_states (
                 strategy_id TEXT NOT NULL,
                 direction TEXT NOT NULL,
                 released INTEGER NOT NULL,
                 released_at TEXT,
                 first_fill_at TEXT,
                 PRIMARY KEY (strategy_id, direction)
             );
             CREATE TABLE IF NOT EXISTS config_versions (
                 strategy_id TEXT NOT NULL,
                 version INTEGER NOT NULL,
                 config_json TEXT NOT NULL,
                 recorded_at TEXT NOT NULL,
                 PRIMARY KEY (strategy_id, version)
             );
             CREATE TABLE IF NOT EXISTS strategy_views (
                 strategy_id TEXT PRIMARY KEY,
                 taken_at TEXT NOT NULL,
                 view_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS account_settings (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 settings_json TEXT NOT NULL
             );";

impl Store {
    /// 建策略：策略记录、初始状态、一个参数版本和第一份视图快照写在同一个事务里。
    ///
    /// 分开写就可能留下「策略已存在、格子状态是空的」的记录，这种策略既跑不起来
    /// 也暂停不了（每次读状态都会因为格数对不上而失败）。视图快照同理：列表和详情
    /// 只读快照，缺一份就什么都显示不出来。
    pub fn insert_strategy(
        &self,
        strategy: &StoredStrategy,
        state: &StrategyState,
        view: &ViewSnapshot,
    ) -> Result<(), StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        write_strategy(&transaction, strategy, state, view)?;
        transaction.commit()?;
        Ok(())
    }

    /// 草稿确认启动：写入策略记录、初始状态和一个参数版本，并删掉草稿，同一个事务。
    ///
    /// 分开写会留下两种中间态：草稿没了、策略没建起来（用户以为启动了，其实什么都没有），
    /// 或者策略建起来了、草稿还在（同一个参数能再启动一次）。
    ///
    /// 一个标的同时只能有一个活动策略，所以「这个标的上还没有活动策略」这个检查也在
    /// 同一个事务里做（B46）：放到事务外面，两个不同草稿可以各自通过检查，再各自建出
    /// 一个策略，同一个标的上就有两个策略在挂单。
    pub fn start_draft(
        &self,
        draft_id: &StrategyId,
        strategy: &StoredStrategy,
        state: &StrategyState,
        view: &ViewSnapshot,
    ) -> Result<(), StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        if let Some(strategy_id) = live_strategy_on(&transaction, &strategy.instrument)? {
            return Err(StoreError::InstrumentBusy {
                instrument: strategy.instrument.clone(),
                strategy_id,
            });
        }
        write_strategy(&transaction, strategy, state, view)?;
        transaction.execute(
            "DELETE FROM drafts WHERE draft_id = ?1",
            params![draft_id.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// 存一个草稿：草稿记录和第一个参数版本写在同一个事务里。
    pub fn insert_draft(&self, draft: &StoredDraft) -> Result<(), StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO drafts (draft_id, config_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                draft.draft_id.as_str(),
                serde_json::to_string(&draft.config)?,
                format_time(draft.created_at),
                format_time(draft.updated_at),
            ],
        )?;
        insert_config_version(
            &transaction,
            &draft.draft_id,
            1,
            &draft.config,
            draft.created_at,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// 保存草稿改过的参数，并记一个新的参数版本，返回新版本号（第 2.4 节）。
    pub fn save_draft_config(
        &self,
        draft_id: &StrategyId,
        config: &StrategyConfig,
        at: DateTime<Utc>,
    ) -> Result<i64, StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let updated = transaction.execute(
            "UPDATE drafts SET config_json = ?2, updated_at = ?3 WHERE draft_id = ?1",
            params![
                draft_id.as_str(),
                serde_json::to_string(config)?,
                format_time(at),
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::UnknownDraft(draft_id.clone()));
        }
        let version = record_config_version(&transaction, draft_id, config, at)?;
        transaction.commit()?;
        Ok(version)
    }

    pub fn load_draft(&self, draft_id: &StrategyId) -> Result<StoredDraft, StoreError> {
        let connection = self.lock();
        let row = connection
            .query_row(
                "SELECT config_json, created_at, updated_at FROM drafts WHERE draft_id = ?1",
                params![draft_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((config_json, created_at, updated_at)) = row else {
            return Err(StoreError::UnknownDraft(draft_id.clone()));
        };
        Ok(StoredDraft {
            draft_id: draft_id.clone(),
            config: serde_json::from_str(&config_json)?,
            created_at: parse_time(&created_at)?,
            updated_at: parse_time(&updated_at)?,
        })
    }

    /// 删掉一个草稿，连它保存过的参数版本一起删。
    ///
    /// 参数版本是给「这次启动用的是第几版参数」留的，草稿都没了就没有再留着它的理由。
    /// 草稿标识和策略标识共用一套，所以要先确认删掉的确实是一行草稿：删一个碰巧和
    /// 某个策略同名的草稿，不能把那个策略的参数版本抹掉。
    pub fn delete_draft(&self, draft_id: &StrategyId) -> Result<(), StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let removed = transaction.execute(
            "DELETE FROM drafts WHERE draft_id = ?1",
            params![draft_id.as_str()],
        )?;
        if removed > 0 {
            transaction.execute(
                "DELETE FROM config_versions WHERE strategy_id = ?1",
                params![draft_id.as_str()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// 全部草稿，最近建的在前。
    pub fn load_draft_ids(&self) -> Result<Vec<StrategyId>, StoreError> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT draft_id FROM drafts ORDER BY created_at DESC, rowid DESC")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(StrategyId::try_from(row?)?);
        }
        Ok(ids)
    }

    /// 全部策略，最近建的在前。
    pub fn load_all_strategy_ids(&self) -> Result<Vec<StrategyId>, StoreError> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT strategy_id FROM strategies ORDER BY created_at DESC, rowid DESC")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(StrategyId::try_from(row?)?);
        }
        Ok(ids)
    }

    /// 保存运行中改过的配置，并记一个新的参数版本，返回新版本号。
    ///
    /// 配置和版本号写在同一个事务里：改了参数却没有版本记录，事后就说不清哪一段时间
    /// 用的是哪套参数（第 2.4 节）。
    ///
    /// `state` 是要跟着这次改配置一起落盘的状态，没有就传 `None`。加上限速时要重建
    /// 放行进度，它必须和新配置写在同一笔里：分成两次写，中间崩掉就留下「配置已经限速、
    /// 进度还是全部放行」，重启后限速静默失效（第 4.1 节）。
    ///
    /// 只给 runtime 用（改参数要经过 [`crate::Runtime::update_config`] 的检查：只有限速
    /// 间隔能改）。放出去就直接改配置、绕过那些检查了。
    pub(crate) fn save_config(
        &self,
        strategy_id: &StrategyId,
        config: &StrategyConfig,
        state: Option<&StrategyState>,
        at: DateTime<Utc>,
    ) -> Result<i64, StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE strategies SET config_json = ?2, updated_at = ?3 WHERE strategy_id = ?1",
            params![
                strategy_id.as_str(),
                serde_json::to_string(config)?,
                format_time(at),
            ],
        )?;
        let next = record_config_version(&transaction, strategy_id, config, at)?;
        if let Some(state) = state {
            write_state(&transaction, strategy_id, state)?;
        }
        transaction.commit()?;
        Ok(next)
    }

    /// 一个策略当前的参数版本号。
    ///
    /// 策略在建立时就写了第 1 个版本，而且是和策略记录写在同一个事务里，所以读得到
    /// 策略就一定读得到版本号。
    pub fn latest_config_version(&self, strategy_id: &StrategyId) -> Result<i64, StoreError> {
        let connection = self.lock();
        let version = connection.query_row(
            "SELECT MAX(version) FROM config_versions WHERE strategy_id = ?1",
            params![strategy_id.as_str()],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        version.ok_or_else(|| StoreError::UnknownStrategy(strategy_id.clone()))
    }

    /// 一个策略的参数版本，按版本号从小到大。
    pub fn load_config_versions(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Vec<StoredConfigVersion>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT version, config_json, recorded_at FROM config_versions
             WHERE strategy_id = ?1 ORDER BY version",
        )?;
        let rows = statement.query_map(params![strategy_id.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut versions = Vec::new();
        for row in rows {
            let (version, config_json, recorded_at) = row?;
            versions.push(StoredConfigVersion {
                version,
                config: serde_json::from_str(&config_json)?,
                recorded_at: parse_time(&recorded_at)?,
            });
        }
        Ok(versions)
    }

    pub fn load_strategy(&self, strategy_id: &StrategyId) -> Result<StoredStrategy, StoreError> {
        let connection = self.lock();
        let row = connection
            .query_row(
                "SELECT instrument, status, config_json, spec_json, fees_json,
                        fills_synced_through, created_at, updated_at
                 FROM strategies WHERE strategy_id = ?1",
                params![strategy_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            instrument,
            status,
            config_json,
            spec_json,
            fees_json,
            fills_synced_through,
            created_at,
            updated_at,
        )) = row
        else {
            return Err(StoreError::UnknownStrategy(strategy_id.clone()));
        };

        Ok(StoredStrategy {
            strategy_id: strategy_id.clone(),
            instrument: Instrument::try_new(instrument).map_err(|_| StoreError::InvalidColumn {
                column: "strategies.instrument",
                value: String::new(),
            })?,
            status: StrategyStatus::parse(&status)?,
            config: serde_json::from_str(&config_json)?,
            spec: serde_json::from_str(&spec_json)?,
            fees: serde_json::from_str(&fees_json)?,
            fills_synced_through: parse_time(&fills_synced_through)?,
            created_at: parse_time(&created_at)?,
            updated_at: parse_time(&updated_at)?,
        })
    }

    /// 一个标的上的全部策略，最近建的在前。
    pub fn load_strategy_ids(
        &self,
        instrument: &Instrument,
    ) -> Result<Vec<StrategyId>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT strategy_id FROM strategies WHERE instrument = ?1
             ORDER BY created_at DESC, rowid DESC",
        )?;
        let rows =
            statement.query_map(params![instrument.symbol()], |row| row.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(StrategyId::try_from(row?)?);
        }
        Ok(ids)
    }

    pub fn save_strategy_status(
        &self,
        strategy_id: &StrategyId,
        status: StrategyStatus,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.lock().execute(
            "UPDATE strategies SET status = ?2, updated_at = ?3 WHERE strategy_id = ?1",
            params![strategy_id.as_str(), status.as_str(), format_time(now)],
        )?;
        Ok(())
    }

    pub fn save_fills_synced_through(
        &self,
        strategy_id: &StrategyId,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.lock().execute(
            "UPDATE strategies SET fills_synced_through = ?2, updated_at = ?2
             WHERE strategy_id = ?1",
            params![strategy_id.as_str(), format_time(at)],
        )?;
        Ok(())
    }

    /// 落一份视图快照（第 11 节）。
    pub fn save_view(
        &self,
        strategy_id: &StrategyId,
        view: &ViewSnapshot,
    ) -> Result<(), StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        write_view(&transaction, strategy_id, view)?;
        transaction.commit()?;
        Ok(())
    }

    /// 账户风控设置；还没保存过时是默认值（第 7.4 节）。
    pub fn load_account_settings(&self) -> Result<AccountSettings, StoreError> {
        let connection = self.lock();
        let json = connection
            .query_row(
                "SELECT settings_json FROM account_settings WHERE id = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match json {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(AccountSettings::default()),
        }
    }

    /// 保存账户风控设置。调用方负责先校验（[`AccountSettings::validate`]）。
    pub fn save_account_settings(&self, settings: &AccountSettings) -> Result<(), StoreError> {
        self.lock().execute(
            "INSERT INTO account_settings (id, settings_json) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET settings_json = excluded.settings_json",
            params![serde_json::to_string(settings)?],
        )?;
        Ok(())
    }

    /// 读一个策略最近一份视图快照。
    pub fn load_view(&self, strategy_id: &StrategyId) -> Result<ViewSnapshot, StoreError> {
        let connection = self.lock();
        let json = connection
            .query_row(
                "SELECT view_json FROM strategy_views WHERE strategy_id = ?1",
                params![strategy_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Err(StoreError::UnknownView(strategy_id.clone()));
        };
        Ok(serde_json::from_str(&json)?)
    }

    /// 读一个策略的状态：每一侧各批主仓的成交与结束标记、各格子的启用标记与待买回量，
    /// 以及每一侧的限速放行进度。
    pub fn load_state(&self, strategy_id: &StrategyId) -> Result<StrategyState, StoreError> {
        let connection = self.lock();
        let batches = load_batch_states(&connection, strategy_id)?;
        let cells = load_cell_states(&connection, strategy_id)?;
        let directions = directions_of(&batches, &cells);
        let mut sides = Vec::with_capacity(directions.len());
        for direction in directions {
            let mut side = SideState::new(
                direction,
                batches
                    .iter()
                    .filter(|(side, _)| *side == direction)
                    .map(|(_, batch)| batch.clone())
                    .collect(),
                cells
                    .iter()
                    .filter(|(side, _)| *side == direction)
                    .map(|(_, cell)| cell.clone())
                    .collect(),
            );
            // 限速的放行进度（第 12 节）。没有这一行时按「一个单位都没放行」处理：
            // 下一步会按当时的时间重新放行，不会卡住。
            if let Some(throttle) = load_throttle(&connection, strategy_id, direction)? {
                side.set_throttle(throttle);
            }
            sides.push(side);
        }
        Ok(StrategyState::new(sides))
    }

    /// 把一次成交记账的结果落盘：策略状态和订单记录写在同一个事务里。
    ///
    /// 策略状态和订单的累计成交量是同一段成交历史的两种表示，分开写就可能出现
    /// 「订单已成交、状态没变」的不一致。这里保证要么都写进去，要么都没写。
    pub fn save_applied_fills(
        &self,
        strategy_id: &StrategyId,
        state: &StrategyState,
        orders: &[&StoredOrder],
    ) -> Result<(), StoreError> {
        if self.take_injected_write_failure() {
            return Err(StoreError::InjectedWriteFailure);
        }
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        write_state(&transaction, strategy_id, state)?;
        {
            let mut statement = transaction.prepare(
                "UPDATE orders SET
                     cumulative_filled_units = ?3, status = ?4, exchange_order_id = ?5,
                     fills_reconciled = ?6, updated_at = ?7
                 WHERE client_order_id = ?1 AND strategy_id = ?2",
            )?;
            for order in orders {
                statement.execute(params![
                    order.record.client_order_id.as_str(),
                    strategy_id.as_str(),
                    order.record.cumulative_filled.units(),
                    order.record.status.as_str(),
                    order.exchange_order_id,
                    i64::from(order.fills_reconciled),
                    format_time(order.updated_at),
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// 写入一张新订单。发送给交易所之前调用。
    pub fn insert_order(
        &self,
        strategy_id: &StrategyId,
        order: &StoredOrder,
    ) -> Result<(), StoreError> {
        let record = &order.record;
        let (unit_kind, unit_direction, unit_index) = unit_columns(record.unit);
        self.lock().execute(
            "INSERT INTO orders (
                 client_order_id, strategy_id, unit_kind, unit_direction, unit_index, purpose,
                 limit_price_units, quantity_units, cumulative_filled_units, reduce_only, status,
                 fills_reconciled, exchange_order_id, expires_at, placed_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                record.client_order_id.as_str(),
                strategy_id.as_str(),
                unit_kind,
                unit_direction,
                unit_index,
                record.purpose.as_str(),
                record.limit_price.units(),
                record.quantity.units(),
                record.cumulative_filled.units(),
                i64::from(record.reduce_only),
                record.status.as_str(),
                i64::from(order.fills_reconciled),
                order.exchange_order_id,
                format_time(order.expires_at),
                format_time(order.placed_at),
                format_time(order.updated_at),
            ],
        )?;
        Ok(())
    }

    /// 更新一张订单。`expires_at` 一起写：发送之前可能按当时的时钟重算它。
    pub fn update_order(
        &self,
        strategy_id: &StrategyId,
        order: &StoredOrder,
    ) -> Result<(), StoreError> {
        {
            let mut injected = self
                .injected_order_failure
                .lock()
                .expect("injection lock poisoned");
            if injected.as_ref().is_some_and(|matching| matching(order)) {
                *injected = None;
                return Err(StoreError::InjectedWriteFailure);
            }
        }
        let record = &order.record;
        self.lock().execute(
            "UPDATE orders SET
                 cumulative_filled_units = ?3, status = ?4, exchange_order_id = ?5,
                 fills_reconciled = ?6, expires_at = ?7, updated_at = ?8
             WHERE client_order_id = ?1 AND strategy_id = ?2",
            params![
                record.client_order_id.as_str(),
                strategy_id.as_str(),
                record.cumulative_filled.units(),
                record.status.as_str(),
                order.exchange_order_id,
                i64::from(order.fills_reconciled),
                format_time(order.expires_at),
                format_time(order.updated_at),
            ],
        )?;
        Ok(())
    }

    /// 删掉一张还没发出去的订单。
    ///
    /// 发送之前先写库（第 9.1 节），但写库本身也花时间：写完发现有效期已经不够、
    /// 这一步不发送时，把这条记录删掉，订单回到「还没发送、也可能没发送」之前的样子，
    /// 下一步重新规划。删除只对从未发出去的订单用，交易所那边没有它。
    pub fn delete_order(
        &self,
        strategy_id: &StrategyId,
        client_order_id: &ClientOrderId,
    ) -> Result<(), StoreError> {
        self.lock().execute(
            "DELETE FROM orders WHERE client_order_id = ?1 AND strategy_id = ?2",
            params![client_order_id.as_str(), strategy_id.as_str()],
        )?;
        Ok(())
    }

    /// 这张订单是否已经落库。用来在发送之前断言「先写库、后发送」。
    ///
    /// 客户端订单 ID 是主键，不需要再按策略过滤。
    pub fn has_order(&self, client_order_id: &ClientOrderId) -> Result<bool, StoreError> {
        let found: Option<i64> = self
            .lock()
            .query_row(
                "SELECT 1 FROM orders WHERE client_order_id = ?1",
                params![client_order_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// 成交流水表里每张订单的数量之和，按成交编号去重。
    ///
    /// 判断一张订单的成交有没有对完账时要拿它和订单记入的累计成交量比：
    /// 订单状态来自交易所，流水来自逐笔成交，两者可能不同步（第 9.2 节）。
    ///
    /// `trade_id` 是流水表的主键，同一笔成交只可能有一行，所以这里的求和天然是
    /// 「按成交编号去重」的：同一笔成交被重复报告不会把和抬高，也就不能填补
    /// 另一笔真正缺失的成交。
    pub fn fills_totals(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<HashMap<ClientOrderId, Quantity>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT client_order_id, SUM(quantity_units) FROM fills
             WHERE strategy_id = ?1 GROUP BY client_order_id",
        )?;
        let rows = statement.query_map(params![strategy_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut totals = HashMap::new();
        for row in rows {
            let (client_order_id, units) = row?;
            let client_order_id =
                ClientOrderId::try_new(client_order_id.clone()).map_err(|_| {
                    StoreError::InvalidColumn {
                        column: "fills.client_order_id",
                        value: client_order_id,
                    }
                })?;
            totals.insert(client_order_id, Quantity::from_units(units));
        }
        Ok(totals)
    }

    /// 让之后每一次写订单都失败，用来构造存储故障。
    ///
    /// 只给集成测试用：集成测试是另一个 crate，看不到 `#[cfg(test)]` 的东西。
    /// 正式流程不用。
    #[doc(hidden)]
    pub fn fail_order_writes_for_test(&self) -> Result<(), StoreError> {
        self.lock().execute_batch(
            "CREATE TRIGGER orders_write_fails BEFORE INSERT ON orders
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )?;
        Ok(())
    }

    /// 让之后每一次写策略状态都失败，用来构造存储故障。
    ///
    /// 只给集成测试用：集成测试是另一个 crate，看不到 `#[cfg(test)]` 的东西。
    /// 正式流程不用。
    #[doc(hidden)]
    pub fn fail_state_writes_for_test(&self) -> Result<(), StoreError> {
        // 限速进度是每次写状态都要改的一行（[`write_state`] 里按主键 upsert），
        // 拦住它的插入和更新就等于拦住整笔状态写入。
        self.lock().execute_batch(
            "CREATE TRIGGER throttle_state_insert_fails BEFORE INSERT ON throttle_states
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;
             CREATE TRIGGER throttle_state_update_fails BEFORE UPDATE ON throttle_states
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )?;
        Ok(())
    }

    /// 按挂单先后返回一个策略的所有订单。
    pub fn load_orders(&self, strategy_id: &StrategyId) -> Result<Vec<StoredOrder>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT client_order_id, unit_kind, unit_direction, unit_index, purpose,
                    limit_price_units, quantity_units, cumulative_filled_units, reduce_only, status,
                    fills_reconciled, exchange_order_id, expires_at, placed_at, updated_at
             FROM orders WHERE strategy_id = ?1 ORDER BY placed_at, rowid",
        )?;
        let rows = statement.query_map(params![strategy_id.as_str()], |row| {
            Ok(StoredOrderRow {
                client_order_id: row.get(0)?,
                unit_kind: row.get(1)?,
                unit_direction: row.get(2)?,
                unit_index: row.get(3)?,
                purpose: row.get(4)?,
                limit_price_units: row.get(5)?,
                quantity_units: row.get(6)?,
                cumulative_filled_units: row.get(7)?,
                reduce_only: row.get(8)?,
                status: row.get(9)?,
                fills_reconciled: row.get(10)?,
                exchange_order_id: row.get(11)?,
                expires_at: row.get(12)?,
                placed_at: row.get(13)?,
                updated_at: row.get(14)?,
            })
        })?;

        let mut orders = Vec::new();
        for row in rows {
            orders.push(row?.into_stored_order()?);
        }
        Ok(orders)
    }

    /// 记一笔成交；已经记过时返回 `false`。
    pub fn insert_fill(&self, fill: &StoredFill) -> Result<bool, StoreError> {
        let inserted = self.lock().execute(
            "INSERT OR IGNORE INTO fills (
                 trade_id, strategy_id, client_order_id, price_units, quantity_units,
                 cumulative_quantity_units, filled_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                fill.trade_id,
                fill.strategy_id.as_str(),
                fill.client_order_id.as_str(),
                fill.price.units(),
                fill.quantity.units(),
                fill.cumulative_quantity.units(),
                format_time(fill.filled_at),
            ],
        )?;
        Ok(inserted > 0)
    }

    pub fn load_fills(&self, strategy_id: &StrategyId) -> Result<Vec<StoredFill>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT trade_id, client_order_id, price_units, quantity_units,
                    cumulative_quantity_units, filled_at
             FROM fills WHERE strategy_id = ?1 ORDER BY filled_at, rowid",
        )?;
        let rows = statement.query_map(params![strategy_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut fills = Vec::new();
        for row in rows {
            let (trade_id, client_order_id, price, quantity, cumulative, filled_at) = row?;
            fills.push(StoredFill {
                trade_id,
                strategy_id: strategy_id.clone(),
                client_order_id: ClientOrderId::try_new(client_order_id).map_err(|_| {
                    StoreError::InvalidColumn {
                        column: "fills.client_order_id",
                        value: String::new(),
                    }
                })?,
                price: Price::from_units(price),
                quantity: Quantity::from_units(quantity),
                cumulative_quantity: Quantity::from_units(cumulative),
                filled_at: parse_time(&filled_at)?,
            });
        }
        Ok(fills)
    }
}

struct StoredOrderRow {
    client_order_id: String,
    unit_kind: String,
    unit_direction: String,
    unit_index: i64,
    purpose: String,
    limit_price_units: i64,
    quantity_units: i64,
    cumulative_filled_units: i64,
    reduce_only: i64,
    status: String,
    fills_reconciled: i64,
    exchange_order_id: Option<String>,
    expires_at: String,
    placed_at: String,
    updated_at: String,
}

impl StoredOrderRow {
    fn into_stored_order(self) -> Result<StoredOrder, StoreError> {
        Ok(StoredOrder {
            record: OrderRecord {
                client_order_id: ClientOrderId::try_new(self.client_order_id).map_err(|_| {
                    StoreError::InvalidColumn {
                        column: "orders.client_order_id",
                        value: String::new(),
                    }
                })?,
                unit: parse_unit(&self.unit_kind, &self.unit_direction, self.unit_index)?,
                purpose: parse_purpose(&self.purpose)?,
                limit_price: Price::from_units(self.limit_price_units),
                quantity: Quantity::from_units(self.quantity_units),
                cumulative_filled: Quantity::from_units(self.cumulative_filled_units),
                reduce_only: self.reduce_only != 0,
                status: parse_order_status(&self.status)?,
            },
            fills_reconciled: self.fills_reconciled != 0,
            exchange_order_id: self.exchange_order_id,
            expires_at: parse_time(&self.expires_at)?,
            placed_at: parse_time(&self.placed_at)?,
            updated_at: parse_time(&self.updated_at)?,
        })
    }
}

fn parse_purpose(value: &str) -> Result<OrderPurpose, StoreError> {
    match value {
        "core" => Ok(OrderPurpose::Core),
        "grid_entry" => Ok(OrderPurpose::GridEntry),
        "grid_exit" => Ok(OrderPurpose::GridExit),
        other => Err(StoreError::UnknownPurpose {
            value: other.to_string(),
        }),
    }
}

fn parse_direction(value: &str) -> Result<Direction, StoreError> {
    match value {
        "long" => Ok(Direction::Long),
        "short" => Ok(Direction::Short),
        other => Err(StoreError::UnknownDirection {
            value: other.to_string(),
        }),
    }
}

/// 订单归属单位存成三列：种类、方向和下标。
fn unit_columns(unit: Unit) -> (&'static str, &'static str, i64) {
    let direction = unit.direction().as_str();
    match unit {
        Unit::Batch(_, BatchIndex(index)) => ("batch", direction, index as i64),
        Unit::Cell(_, CellIndex(index)) => ("cell", direction, index as i64),
    }
}

fn parse_unit(kind: &str, direction: &str, index: i64) -> Result<Unit, StoreError> {
    let unknown = || StoreError::UnknownUnit {
        value: kind.to_string(),
    };
    let direction = parse_direction(direction)?;
    let index = usize::try_from(index).map_err(|_| unknown())?;
    match kind {
        "batch" => Ok(Unit::Batch(direction, BatchIndex(index))),
        "cell" => Ok(Unit::Cell(direction, CellIndex(index))),
        _ => Err(unknown()),
    }
}

fn parse_order_status(value: &str) -> Result<OrderStatus, StoreError> {
    match value {
        "submitting" => Ok(OrderStatus::Submitting),
        "open" => Ok(OrderStatus::Open),
        "partially_filled" => Ok(OrderStatus::PartiallyFilled),
        "filled" => Ok(OrderStatus::Filled),
        "canceled" => Ok(OrderStatus::Canceled),
        "rejected" => Ok(OrderStatus::Rejected),
        "void" => Ok(OrderStatus::Void),
        "unknown" => Ok(OrderStatus::Unknown),
        other => Err(StoreError::UnknownOrderStatus {
            value: other.to_string(),
        }),
    }
}

/// 全量改写一个策略的状态。
///
/// 两侧的批次和格子都是整体替换的：它们的数量由配置决定，不会中途增减。
fn write_state(
    transaction: &rusqlite::Transaction<'_>,
    strategy_id: &StrategyId,
    state: &StrategyState,
) -> Result<(), StoreError> {
    transaction.execute(
        "DELETE FROM batch_states WHERE strategy_id = ?1",
        params![strategy_id.as_str()],
    )?;
    {
        let mut statement = transaction.prepare(
            "INSERT INTO batch_states (strategy_id, direction, batch_index, filled_units, finished)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for side in state.sides() {
            for (index, batch) in side.batches().iter().enumerate() {
                statement.execute(params![
                    strategy_id.as_str(),
                    side.direction().as_str(),
                    index as i64,
                    batch.filled.units(),
                    i64::from(batch.finished),
                ])?;
            }
        }
    }

    transaction.execute(
        "DELETE FROM cell_states WHERE strategy_id = ?1",
        params![strategy_id.as_str()],
    )?;
    {
        let mut statement = transaction.prepare(
            "INSERT INTO cell_states (strategy_id, direction, cell_index, enabled, to_buy_back_units)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for side in state.sides() {
            for (index, cell) in side.cells().iter().enumerate() {
                statement.execute(params![
                    strategy_id.as_str(),
                    side.direction().as_str(),
                    index as i64,
                    i64::from(cell.enabled),
                    cell.to_buy_back.units(),
                ])?;
            }
        }
    }

    {
        let mut statement = transaction.prepare(
            "INSERT INTO throttle_states (strategy_id, direction, released, released_at, first_fill_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (strategy_id, direction) DO UPDATE SET
                 released = excluded.released,
                 released_at = excluded.released_at,
                 first_fill_at = excluded.first_fill_at",
        )?;
        for side in state.sides() {
            let throttle = side.throttle();
            statement.execute(params![
                strategy_id.as_str(),
                side.direction().as_str(),
                throttle.released as i64,
                throttle.released_at.map(format_time),
                throttle.first_fill_at.map(format_time),
            ])?;
        }
    }
    Ok(())
}

/// 每一侧的批次状态，按方向与批次下标排列。
fn load_batch_states(
    connection: &Connection,
    strategy_id: &StrategyId,
) -> Result<Vec<(Direction, BatchState)>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT direction, filled_units, finished FROM batch_states
         WHERE strategy_id = ?1 ORDER BY direction, batch_index",
    )?;
    let rows = statement.query_map(params![strategy_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut batches = Vec::new();
    for row in rows {
        let (direction, filled, finished) = row?;
        batches.push((
            parse_direction(&direction)?,
            BatchState {
                filled: Quantity::from_units(filled),
                finished: finished != 0,
            },
        ));
    }
    Ok(batches)
}

/// 每一侧的格子状态，按方向与格子下标排列。
fn load_cell_states(
    connection: &Connection,
    strategy_id: &StrategyId,
) -> Result<Vec<(Direction, CellState)>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT direction, enabled, to_buy_back_units FROM cell_states
         WHERE strategy_id = ?1 ORDER BY direction, cell_index",
    )?;
    let rows = statement.query_map(params![strategy_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut cells = Vec::new();
    for row in rows {
        let (direction, enabled, to_buy_back) = row?;
        cells.push((
            parse_direction(&direction)?,
            CellState {
                enabled: enabled != 0,
                to_buy_back: Quantity::from_units(to_buy_back),
            },
        ));
    }
    Ok(cells)
}

/// 一侧的限速放行进度；没有这一行时是 `None`。
fn load_throttle(
    connection: &Connection,
    strategy_id: &StrategyId,
    direction: Direction,
) -> Result<Option<ThrottleState>, StoreError> {
    let throttle = connection
        .query_row(
            "SELECT released, released_at, first_fill_at FROM throttle_states
             WHERE strategy_id = ?1 AND direction = ?2",
            params![strategy_id.as_str(), direction.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((released, released_at, first_fill_at)) = throttle else {
        return Ok(None);
    };
    Ok(Some(ThrottleState {
        released: usize::try_from(released).map_err(|_| StoreError::InvalidColumn {
            column: "throttle_states.released",
            value: released.to_string(),
        })?,
        released_at: released_at.as_deref().map(parse_time).transpose()?,
        first_fill_at: first_fill_at.as_deref().map(parse_time).transpose()?,
    }))
}

/// 状态里出现过的方向，按 [`Direction::ALL`] 的顺序。
///
/// 每一侧至少有一批主仓或者一个格子，所以两个表合起来就能给出配了哪几侧。
fn directions_of(
    batches: &[(Direction, BatchState)],
    cells: &[(Direction, CellState)],
) -> Vec<Direction> {
    Direction::ALL
        .into_iter()
        .filter(|direction| {
            batches.iter().any(|(side, _)| side == direction)
                || cells.iter().any(|(side, _)| side == direction)
        })
        .collect()
}

/// 一个标的上正在活动（运行中或暂停）的策略。
///
/// 活动与否的判定用 [`StrategyStatus::is_live`]，和别处只有这一个定义。
fn live_strategy_on(
    transaction: &rusqlite::Transaction<'_>,
    instrument: &Instrument,
) -> Result<Option<StrategyId>, StoreError> {
    let mut statement =
        transaction.prepare("SELECT strategy_id, status FROM strategies WHERE instrument = ?1")?;
    let rows = statement.query_map(params![instrument.symbol()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (strategy_id, status) = row?;
        if StrategyStatus::parse(&status)?.is_live() {
            return Ok(Some(StrategyId::try_from(strategy_id)?));
        }
    }
    Ok(None)
}

/// 写入一条策略记录、它的初始状态和一个参数版本。
///
/// 版本号接着已有的往下排：草稿保存过几次，确认启动后策略的参数版本就从那之后继续，
/// 而不是重新从 1 开始——同一次启动前后是同一份参数历史。
fn write_strategy(
    transaction: &rusqlite::Transaction<'_>,
    strategy: &StoredStrategy,
    state: &StrategyState,
    view: &ViewSnapshot,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO strategies (
             strategy_id, instrument, status, config_json, spec_json, fees_json,
             fills_synced_through, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            strategy.strategy_id.as_str(),
            strategy.instrument.symbol(),
            strategy.status.as_str(),
            serde_json::to_string(&strategy.config)?,
            serde_json::to_string(&strategy.spec)?,
            serde_json::to_string(&strategy.fees)?,
            format_time(strategy.fills_synced_through),
            format_time(strategy.created_at),
            format_time(strategy.updated_at),
        ],
    )?;
    write_state(transaction, &strategy.strategy_id, state)?;
    write_view(transaction, &strategy.strategy_id, view)?;
    record_config_version(
        transaction,
        &strategy.strategy_id,
        &strategy.config,
        strategy.created_at,
    )
    .map(|_| ())
}

/// 写一份视图快照，覆盖同一策略上一份。
///
/// 不动 `strategies.updated_at`：快照每走一步就落一份，拿它当「策略什么时候变过」
/// 会让那个时间每几秒跳一次，看不出参数和状态是什么时候改的。
fn write_view(
    transaction: &rusqlite::Transaction<'_>,
    strategy_id: &StrategyId,
    view: &ViewSnapshot,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO strategy_views (strategy_id, taken_at, view_json)
         VALUES (?1, ?2, ?3)
         ON CONFLICT (strategy_id) DO UPDATE SET
             taken_at = excluded.taken_at,
             view_json = excluded.view_json",
        params![
            strategy_id.as_str(),
            format_time(view.taken_at),
            serde_json::to_string(view)?,
        ],
    )?;
    Ok(())
}

/// 记一个参数版本，返回这次用的版本号。
///
/// 和最后一个版本完全相同的参数不再记一条，直接沿用它的版本号：参数版本是事后复盘
/// 「哪次改动让成交变了」的依据，同样的参数记两遍会让版本号不再对应不同的参数集。
/// 从草稿启动的策略就是这种情况——最后一次保存之后参数没有动过。
fn record_config_version(
    transaction: &rusqlite::Transaction<'_>,
    id: &StrategyId,
    config: &StrategyConfig,
    at: DateTime<Utc>,
) -> Result<i64, StoreError> {
    let latest = transaction
        .query_row(
            "SELECT version, config_json FROM config_versions WHERE strategy_id = ?1
             ORDER BY version DESC LIMIT 1",
            params![id.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let version = match latest {
        Some((version, json)) if serde_json::from_str::<StrategyConfig>(&json)? == *config => {
            return Ok(version);
        }
        Some((version, _)) => version + 1,
        None => 1,
    };
    insert_config_version(transaction, id, version, config, at)?;
    Ok(version)
}

/// 记一个参数版本。`version` 由调用方给出。
fn insert_config_version(
    transaction: &rusqlite::Transaction<'_>,
    strategy_id: &StrategyId,
    version: i64,
    config: &StrategyConfig,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO config_versions (strategy_id, version, config_json, recorded_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            strategy_id.as_str(),
            version,
            serde_json::to_string(config)?,
            format_time(at),
        ],
    )?;
    Ok(())
}

/// 存成固定精度的 UTC 字符串：既是可读的，也能按字典序比较。
fn format_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn parse_time(value: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| StoreError::InvalidColumn {
            column: "timestamp",
            value: value.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::{
        SCHEMA_VERSION, Store, StoreError, StoredDraft, StoredFill, StoredOrder, StoredStrategy,
        StrategyId, StrategyStatus, ViewSnapshot,
    };
    use chrono::{DateTime, TimeZone, Utc};
    use poise_strategy::batch::BatchIndex;
    use poise_strategy::config::{SideConfig, StrategyConfig};
    use poise_strategy::grid::CellIndex;
    use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
    use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Unit};
    use poise_strategy::side::Direction;
    use poise_strategy::state::{BatchState, CellState, SideState, StrategyState};
    use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap() + chrono::Duration::seconds(seconds)
    }

    fn order(cloid: &str) -> StoredOrder {
        StoredOrder {
            record: OrderRecord {
                client_order_id: ClientOrderId::try_new(cloid).unwrap(),
                unit: Unit::Cell(Direction::Long, CellIndex(0)),
                purpose: OrderPurpose::GridEntry,
                limit_price: Price::parse("95000").unwrap(),
                quantity: Quantity::parse("0.05").unwrap(),
                cumulative_filled: Quantity::ZERO,
                reduce_only: false,
                status: OrderStatus::Submitting,
            },
            fills_reconciled: false,
            exchange_order_id: None,
            expires_at: at(5),
            placed_at: at(0),
            updated_at: at(0),
        }
    }

    /// 读一个库文件里的结构版本。
    fn schema_version(path: &std::path::Path) -> i32 {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    /// 一个「这个版本之前建的」旧库：有表，但没有版本记录。
    fn write_an_old_database(path: &std::path::Path) {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch("CREATE TABLE strategies (strategy_id TEXT PRIMARY KEY);")
            .unwrap();
    }

    /// 新建的库把结构版本写进 `user_version`，再打开时不会把自己认成旧库。
    #[test]
    fn a_new_database_records_the_schema_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poise.sqlite");

        let store = Store::open(&path).unwrap();
        assert_eq!(schema_version(&path), SCHEMA_VERSION);
        drop(store);

        // 关掉再打开：版本对得上，照常打开。
        Store::open(&path).unwrap();
    }

    /// 结构版本对不上的旧库直接拒绝，错误里写清楚库路径、库版本、期望版本和该怎么做。
    ///
    /// 只在写入时才发现少了一列太晚了：那时候用户已经以为策略在跑了（第 12 节）。
    #[test]
    fn a_database_with_another_schema_version_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poise.sqlite");
        write_an_old_database(&path);

        let Err(error) = Store::open(&path) else {
            panic!("an old database must be refused");
        };

        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("version 0"), "{message}");
        assert!(
            message.contains(&format!("version {SCHEMA_VERSION}")),
            "{message}"
        );
        assert!(message.contains("move the old database away"), "{message}");
        // 拒绝就是拒绝：没有顺手把旧库改掉。
        assert_eq!(schema_version(&path), 0);
    }

    /// 比这个版本新的库也拒绝：新版本可能加了这个版本读不懂的东西。
    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poise.sqlite");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
            .unwrap();
        drop(connection);

        let Err(error) = Store::open(&path) else {
            panic!("a newer database must be refused");
        };

        assert!(
            error
                .to_string()
                .contains(&format!("version {}", SCHEMA_VERSION + 1)),
            "{error}"
        );
    }

    /// 建表中途失败不留半成品：重开还能从零建起来。
    ///
    /// 建表和写版本分成两步的话，建表成功之后、版本写下去之前进程退出，会留下一个
    /// 「版本 0、有表」的库。下一次打开被当成旧库拒绝，可它其实是这一版的库，只是没建完，
    /// 用户只能删库重来。放进同一个事务，中途失败就一起回滚。
    #[test]
    fn a_table_creation_that_fails_partway_leaves_the_database_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poise.sqlite");
        // 一个不是表的对象占住 `orders_by_strategy` 这个名字：建到那一条语句就会失败，
        // 前面的表都已经建过了。视图不算表，所以它不会在版本核对那一步被当成旧库挡下来。
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE VIEW orders_by_strategy AS SELECT 1 AS x;")
            .unwrap();
        drop(connection);

        assert!(Store::open(&path).is_err(), "建表失败要报出来");

        // 失败那一次建的表全部回滚，版本也没写：它还是那个「全新的空库」。
        assert_eq!(schema_version(&path), 0);
        let connection = rusqlite::Connection::open(&path).unwrap();
        let tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "建了一半的表要一起回滚");
        // 挡路的对象挪走之后重开：正常建起来，版本也写上了。
        connection
            .execute_batch("DROP VIEW orders_by_strategy;")
            .unwrap();
        drop(connection);

        Store::open(&path).unwrap();
        assert_eq!(schema_version(&path), SCHEMA_VERSION);
    }

    #[test]
    fn orders_are_written_before_they_are_sent_and_survive_reopening() {
        let strategy_id = StrategyId::try_from("s-test".to_string()).unwrap();
        let order = order("0x01");

        {
            let store = Store::open_in_memory().unwrap();
            store.insert_order(&strategy_id, &order).unwrap();
            let stored = store.load_orders(&strategy_id).unwrap();
            assert_eq!(stored, vec![order.clone()]);
            assert_eq!(stored[0].record.status, OrderStatus::Submitting);
            assert_eq!(
                store
                    .load_orders(&StrategyId::try_from("s-other".to_string()).unwrap())
                    .unwrap(),
                vec![]
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poise.sqlite");
        let store = Store::open(&path).unwrap();
        store.insert_order(&strategy_id, &order).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.load_orders(&strategy_id).unwrap(), vec![order]);
    }

    #[test]
    fn fills_are_recorded_once_by_trade_id() {
        let strategy_id = StrategyId::try_from("s-test".to_string()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let fill = StoredFill {
            trade_id: "trade-1".to_string(),
            strategy_id: strategy_id.clone(),
            client_order_id: ClientOrderId::try_new("0x01").unwrap(),
            price: Price::parse("95000").unwrap(),
            quantity: Quantity::parse("0.02").unwrap(),
            cumulative_quantity: Quantity::parse("0.02").unwrap(),
            filled_at: at(1),
        };

        assert!(store.insert_fill(&fill).unwrap());
        assert!(!store.insert_fill(&fill).unwrap());
        assert_eq!(store.load_fills(&strategy_id).unwrap(), vec![fill]);
    }

    /// 流水表里每张订单的数量之和：用来判断订单的成交是不是都已经记进流水。
    #[test]
    fn fill_totals_are_summed_per_order() {
        let strategy_id = StrategyId::try_from("s-test".to_string()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let fill = |trade_id: &str, cloid: &str, quantity: &str, cumulative: &str| StoredFill {
            trade_id: trade_id.to_string(),
            strategy_id: strategy_id.clone(),
            client_order_id: ClientOrderId::try_new(cloid).unwrap(),
            price: Price::parse("95000").unwrap(),
            quantity: Quantity::parse(quantity).unwrap(),
            cumulative_quantity: Quantity::parse(cumulative).unwrap(),
            filled_at: at(1),
        };

        // 同一张订单的两笔成交加在一起；累计量回退的迟到成交也照加。
        store
            .insert_fill(&fill("trade-1", "0x01", "0.02", "0.02"))
            .unwrap();
        store
            .insert_fill(&fill("trade-2", "0x01", "0.03", "0.03"))
            .unwrap();
        store
            .insert_fill(&fill("trade-3", "0x02", "0.01", "0.01"))
            .unwrap();

        let totals = store.fills_totals(&strategy_id).unwrap();
        assert_eq!(
            totals.get(&ClientOrderId::try_new("0x01").unwrap()),
            Some(&Quantity::parse("0.05").unwrap())
        );
        assert_eq!(
            totals.get(&ClientOrderId::try_new("0x02").unwrap()),
            Some(&Quantity::parse("0.01").unwrap())
        );
        // 没有流水的订单不在表里，调用方按零处理。
        assert_eq!(totals.get(&ClientOrderId::try_new("0x03").unwrap()), None);
    }

    #[test]
    fn unknown_strategy_is_an_error() {
        let store = Store::open_in_memory().unwrap();

        let error = store
            .load_strategy(&StrategyId::try_from("s-missing".to_string()).unwrap())
            .unwrap_err();

        assert!(matches!(error, StoreError::UnknownStrategy(_)));
        assert!(StrategyStatus::parse("archived").is_err());
    }

    /// 七种状态都能存下来再读回去（第 2.4 节）。
    #[test]
    fn every_lifecycle_status_round_trips() {
        for status in [
            StrategyStatus::Draft,
            StrategyStatus::Running,
            StrategyStatus::Restoring,
            StrategyStatus::Pausing,
            StrategyStatus::Paused,
            StrategyStatus::Stopping,
            StrategyStatus::Stopped,
        ] {
            assert_eq!(StrategyStatus::parse(status.as_str()).unwrap(), status);
        }
        // 还在交易所里活动的状态都占着这个标的，只撤单的过渡状态和恢复中也算。
        assert!(StrategyStatus::Running.is_live());
        assert!(StrategyStatus::Restoring.is_live());
        assert!(StrategyStatus::Pausing.is_live());
        assert!(StrategyStatus::Paused.is_live());
        assert!(StrategyStatus::Stopping.is_live());
        assert!(!StrategyStatus::Stopped.is_live());
        assert!(!StrategyStatus::Draft.is_live());
        // 只撤单的只有两个过渡状态。
        assert!(StrategyStatus::Pausing.is_cancel_only());
        assert!(StrategyStatus::Stopping.is_cancel_only());
        assert!(!StrategyStatus::Restoring.is_cancel_only());
        assert!(!StrategyStatus::Running.is_cancel_only());
        assert!(!StrategyStatus::Paused.is_cancel_only());
        assert!(!StrategyStatus::Stopped.is_cancel_only());
    }

    /// 视图快照跟着策略一起建起来，之后每落一份就覆盖上一份（第 11 节）。
    #[test]
    fn the_view_snapshot_is_written_with_the_strategy_and_updated_in_place() {
        let store = Store::open_in_memory().unwrap();
        let strategy_id = StrategyId::try_from("s-view".to_string()).unwrap();
        let config = long_grid_config();
        let view = view_at(at(0), StrategyStatus::Running);
        store
            .insert_strategy(
                &StoredStrategy {
                    strategy_id: strategy_id.clone(),
                    instrument: btc(),
                    status: StrategyStatus::Running,
                    config: config.clone(),
                    spec: btc_spec(),
                    fees: btc_fees(),
                    fills_synced_through: at(0),
                    created_at: at(0),
                    updated_at: at(0),
                },
                &initial_state(&config),
                &view,
            )
            .unwrap();
        assert_eq!(store.load_view(&strategy_id).unwrap(), view);

        let later = ViewSnapshot {
            taken_at: at(30),
            status: StrategyStatus::Pausing,
            note: Some("还有 2 张订单没撤掉，正在重试。".to_string()),
            position: Position::parse("0.5").unwrap(),
            ..view.clone()
        };
        store.save_view(&strategy_id, &later).unwrap();

        let loaded = store.load_view(&strategy_id).unwrap();
        assert_eq!(loaded, later);
        // 快照是覆盖，不是追加：同一个策略只有一份。
        assert_eq!(
            loaded.note.as_deref(),
            Some("还有 2 张订单没撤掉，正在重试。")
        );
        // 落快照不算「策略变过」：参数和状态的改动时间不能被它推着走。
        assert_eq!(store.load_strategy(&strategy_id).unwrap().updated_at, at(0));
    }

    #[test]
    fn a_strategy_without_a_view_snapshot_is_an_error() {
        let store = Store::open_in_memory().unwrap();

        let error = store
            .load_view(&StrategyId::try_from("s-missing".to_string()).unwrap())
            .unwrap_err();

        assert!(matches!(error, StoreError::UnknownView(_)));
    }

    /// B42：草稿每次保存记一个版本；确认启动后策略沿用同一份参数历史，草稿消失。
    #[test]
    fn b42_a_draft_keeps_every_saved_version_and_hands_them_to_the_strategy() {
        let draft_id = StrategyId::try_from("s-draft".to_string()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let config = long_grid_config();
        store
            .insert_draft(&StoredDraft {
                draft_id: draft_id.clone(),
                config: config.clone(),
                created_at: at(0),
                updated_at: at(0),
            })
            .unwrap();

        // 两次保存各记一个版本。
        let mut second = config.clone();
        long_side_mut(&mut second).grid_spacing = Price::parse("500").unwrap();
        assert_eq!(
            store.save_draft_config(&draft_id, &second, at(10)).unwrap(),
            2
        );
        let mut third = second.clone();
        long_side_mut(&mut third).throttle_interval = Some(chrono::Duration::minutes(60));
        assert_eq!(
            store.save_draft_config(&draft_id, &third, at(20)).unwrap(),
            3
        );
        assert_eq!(store.load_draft(&draft_id).unwrap().config, third);
        assert_eq!(store.load_draft(&draft_id).unwrap().updated_at, at(20));

        // 确认启动：策略接过草稿的标识和版本历史，草稿不再存在。
        let strategy = StoredStrategy {
            strategy_id: draft_id.clone(),
            instrument: btc(),
            status: StrategyStatus::Running,
            config: third.clone(),
            spec: btc_spec(),
            fees: btc_fees(),
            fills_synced_through: at(30),
            created_at: at(30),
            updated_at: at(30),
        };
        store
            .start_draft(
                &draft_id,
                &strategy,
                &initial_state(&third),
                &view_at(at(30), StrategyStatus::Running),
            )
            .unwrap();

        assert!(matches!(
            store.load_draft(&draft_id).unwrap_err(),
            StoreError::UnknownDraft(_)
        ));
        assert_eq!(store.load_strategy(&draft_id).unwrap().config, third);
        assert_eq!(store.latest_config_version(&draft_id).unwrap(), 3);
        assert_eq!(store.load_config_versions(&draft_id).unwrap().len(), 3);
        assert_eq!(store.load_draft_ids().unwrap(), vec![]);
    }

    /// 直接建的策略没有草稿历史，参数版本从 1 开始。
    #[test]
    fn a_strategy_without_a_draft_starts_at_version_one() {
        let strategy_id = StrategyId::try_from("s-direct".to_string()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let config = long_grid_config();
        store
            .insert_strategy(
                &StoredStrategy {
                    strategy_id: strategy_id.clone(),
                    instrument: btc(),
                    status: StrategyStatus::Running,
                    config: config.clone(),
                    spec: btc_spec(),
                    fees: btc_fees(),
                    fills_synced_through: at(0),
                    created_at: at(0),
                    updated_at: at(0),
                },
                &initial_state(&config),
                &view_at(at(0), StrategyStatus::Running),
            )
            .unwrap();

        assert_eq!(store.latest_config_version(&strategy_id).unwrap(), 1);
    }

    /// 草稿按建立时间倒序列出；不存在的草稿读写都报错。
    #[test]
    fn drafts_are_listed_newest_first() {
        let store = Store::open_in_memory().unwrap();
        for (id, seconds) in [("s-a", 0), ("s-b", 10)] {
            store
                .insert_draft(&StoredDraft {
                    draft_id: StrategyId::try_from(id.to_string()).unwrap(),
                    config: long_grid_config(),
                    created_at: at(seconds),
                    updated_at: at(seconds),
                })
                .unwrap();
        }

        let ids: Vec<String> = store
            .load_draft_ids()
            .unwrap()
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();
        assert_eq!(ids, vec!["s-b", "s-a"]);

        let missing = StrategyId::try_from("s-missing".to_string()).unwrap();
        assert!(matches!(
            store.load_draft(&missing).unwrap_err(),
            StoreError::UnknownDraft(_)
        ));
        assert!(matches!(
            store.save_draft_config(&missing, &long_grid_config(), at(0)),
            Err(StoreError::UnknownDraft(_))
        ));
        // 删一个不存在的草稿不是错误：删除是幂等的。
        store.delete_draft(&missing).unwrap();
    }

    /// 删掉草稿要连它保存过的参数版本一起删。
    ///
    /// 参数版本按标识存，草稿删了版本还在，下一个用同一个标识的草稿就会从第 4 版
    /// 开始，版本号不再对应「这套参数改过几次」。
    #[test]
    fn deleting_a_draft_also_drops_its_config_versions() {
        let store = Store::open_in_memory().unwrap();
        let draft_id = StrategyId::try_from("s-again".to_string()).unwrap();
        let draft = StoredDraft {
            draft_id: draft_id.clone(),
            config: long_grid_config(),
            created_at: at(0),
            updated_at: at(0),
        };
        store.insert_draft(&draft).unwrap();
        let mut changed = long_grid_config();
        long_side_mut(&mut changed).grid_spacing = Price::parse("2000").unwrap();
        assert_eq!(
            store
                .save_draft_config(&draft_id, &changed, at(10))
                .unwrap(),
            2
        );

        store.delete_draft(&draft_id).unwrap();
        assert!(store.load_config_versions(&draft_id).unwrap().is_empty());

        // 再建同标识的草稿：版本从 1 重新开始。
        store.insert_draft(&draft).unwrap();
        assert_eq!(store.latest_config_version(&draft_id).unwrap(), 1);
    }

    /// 删一个不是草稿的标识：什么也不删。
    ///
    /// 草稿标识和策略标识共用一套，删除草稿的接口拿到一个策略标识时，不能把那个
    /// 策略的参数版本抹掉——没有版本记录的策略连列表都读不出来。
    #[test]
    fn deleting_a_draft_leaves_a_strategy_with_the_same_id_alone() {
        let store = Store::open_in_memory().unwrap();
        let strategy_id = StrategyId::try_from("s-live".to_string()).unwrap();
        let config = long_grid_config();
        store
            .insert_strategy(
                &StoredStrategy {
                    strategy_id: strategy_id.clone(),
                    instrument: btc(),
                    status: StrategyStatus::Running,
                    config: config.clone(),
                    spec: btc_spec(),
                    fees: btc_fees(),
                    fills_synced_through: at(0),
                    created_at: at(0),
                    updated_at: at(0),
                },
                &initial_state(&config),
                &view_at(at(0), StrategyStatus::Running),
            )
            .unwrap();

        store.delete_draft(&strategy_id).unwrap();

        assert_eq!(store.latest_config_version(&strategy_id).unwrap(), 1);
        assert_eq!(store.load_all_strategy_ids().unwrap(), vec![strategy_id]);
    }

    /// 同一标的已有活动策略时，写策略的那个事务自己拒绝（B46）。
    ///
    /// 检查必须和写入在同一个事务里：分成两步，两个草稿同时确认就可能各自先查一遍、
    /// 都查不到，然后都写进去。
    #[test]
    fn start_draft_refuses_a_second_live_strategy_on_the_same_instrument() {
        let store = Store::open_in_memory().unwrap();
        let first = StrategyId::try_from("s-first".to_string()).unwrap();
        let second = StrategyId::try_from("s-second".to_string()).unwrap();
        for id in [&first, &second] {
            store
                .insert_draft(&StoredDraft {
                    draft_id: id.clone(),
                    config: long_grid_config(),
                    created_at: at(0),
                    updated_at: at(0),
                })
                .unwrap();
        }
        let config = long_grid_config();
        let stored = |id: &StrategyId| StoredStrategy {
            strategy_id: id.clone(),
            instrument: btc(),
            status: StrategyStatus::Running,
            config: config.clone(),
            spec: btc_spec(),
            fees: btc_fees(),
            fills_synced_through: at(0),
            created_at: at(0),
            updated_at: at(0),
        };

        store
            .start_draft(
                &first,
                &stored(&first),
                &initial_state(&config),
                &view_at(at(0), StrategyStatus::Running),
            )
            .unwrap();
        let error = store
            .start_draft(
                &second,
                &stored(&second),
                &initial_state(&config),
                &view_at(at(0), StrategyStatus::Running),
            )
            .unwrap_err();
        assert!(
            matches!(error, StoreError::InstrumentBusy { .. }),
            "{error:?}"
        );
        // 被拒之后：第二个草稿还在，第一个策略没被动过。
        assert!(store.load_draft(&second).is_ok());
        assert_eq!(store.load_all_strategy_ids().unwrap(), vec![first.clone()]);
        assert_eq!(store.load_draft_ids().unwrap(), vec![second.clone()]);
    }

    fn initial_state(config: &StrategyConfig) -> StrategyState {
        poise_strategy::Strategy::new(config.clone(), btc_spec(), btc_fees())
            .unwrap()
            .initial_state()
    }

    /// 一份没有特殊情况的视图快照：确认启动那一刻读到的数。
    fn view_at(taken_at: DateTime<Utc>, status: StrategyStatus) -> ViewSnapshot {
        ViewSnapshot {
            taken_at,
            status,
            note: None,
            market_price: Price::parse("95500").unwrap(),
            position: Position::ZERO,
            expected_position: Position::ZERO,
            available_collateral: Notional::parse("100000").unwrap(),
            margin_ratio: None,
            waiting: Vec::new(),
        }
    }

    fn btc() -> Instrument {
        Instrument::try_new("BTC").unwrap()
    }

    fn btc_spec() -> InstrumentSpec {
        InstrumentSpec::new(
            btc(),
            Price::parse("1").unwrap(),
            Quantity::parse("0.001").unwrap(),
            Notional::parse("10").unwrap(),
        )
        .unwrap()
    }

    fn btc_fees() -> Fees {
        Fees::new(
            Rate::parse("0.00045").unwrap(),
            Rate::parse("0.00015").unwrap(),
        )
    }

    fn long_grid_config() -> StrategyConfig {
        StrategyConfig {
            instrument: btc(),
            long: Some(SideConfig {
                price_low: Price::parse("75000").unwrap(),
                price_high: Price::parse("95000").unwrap(),
                max_position: Quantity::parse("1").unwrap(),
                core_ratio: Rate::ZERO,
                batch_count: None,
                grid_spacing: Price::parse("1000").unwrap(),
                grid_spread: Price::parse("1000").unwrap(),
                throttle_interval: None,
            }),
            short: None,
        }
    }

    /// 多头侧的配置，没配就 panic。
    fn long_side_mut(config: &mut StrategyConfig) -> &mut SideConfig {
        config.long.as_mut().expect("the long side is configured")
    }

    #[test]
    fn an_order_keeps_the_unit_it_belongs_to() {
        let strategy_id = StrategyId::try_from("s-test".to_string()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut batch_order = order("0x02");
        batch_order.record.unit = Unit::Batch(Direction::Long, BatchIndex(3));
        batch_order.record.purpose = OrderPurpose::Core;

        store.insert_order(&strategy_id, &order("0x01")).unwrap();
        store.insert_order(&strategy_id, &batch_order).unwrap();

        let stored = store.load_orders(&strategy_id).unwrap();
        assert_eq!(
            stored[0].record.unit,
            Unit::Cell(Direction::Long, CellIndex(0))
        );
        assert_eq!(
            stored[1].record.unit,
            Unit::Batch(Direction::Long, BatchIndex(3))
        );
        assert_eq!(stored[1].record.purpose, OrderPurpose::Core);
    }

    #[test]
    fn batch_and_cell_state_survive_reopening() {
        let strategy_id = StrategyId::try_from("s-test".to_string()).unwrap();
        let state = StrategyState::new(vec![SideState::new(
            Direction::Long,
            vec![
                BatchState {
                    filled: Quantity::parse("0.07").unwrap(),
                    finished: false,
                },
                BatchState {
                    filled: Quantity::parse("0.175").unwrap(),
                    finished: true,
                },
            ],
            vec![
                CellState {
                    enabled: true,
                    to_buy_back: Quantity::parse("0.01").unwrap(),
                },
                CellState {
                    enabled: false,
                    to_buy_back: Quantity::parse("0.015").unwrap(),
                },
            ],
        )]);

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poise.sqlite");
        let store = Store::open(&path).unwrap();
        store.save_applied_fills(&strategy_id, &state, &[]).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.load_state(&strategy_id).unwrap(), state);
    }
}
