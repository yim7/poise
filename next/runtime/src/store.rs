//! SQLite 持久化。
//!
//! 订单在发送给交易所之前先写进这里；策略、格子状态、成交记录也都在这里。
//! 数值一律按定点整数单位存，时间存成可比较的 UTC 字符串。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};
use poise_strategy::batch::BatchIndex;
use poise_strategy::config::StrategyConfig;
use poise_strategy::grid::CellIndex;
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Unit};
use poise_strategy::state::{BatchState, CellState, StrategyState};
use poise_strategy::units::{Price, Quantity};
use rusqlite::{Connection, OptionalExtension, params};

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

/// 策略生命周期状态。
///
/// 设计里还有草稿、已停止、已失效；本任务只会产生运行中和已暂停两种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyStatus {
    Running,
    Paused,
}

impl StrategyStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            other => Err(StoreError::UnknownStatus {
                value: other.to_string(),
            }),
        }
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// 一笔已补记的成交。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFill {
    pub trade_id: String,
    pub strategy_id: StrategyId,
    pub client_order_id: ClientOrderId,
    pub price: Price,
    pub quantity: Quantity,
    pub cumulative_quantity: Quantity,
    pub filled_at: DateTime<Utc>,
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
    #[error("unknown strategy status `{value}`")]
    UnknownStatus { value: String },
    #[error("unknown order purpose `{value}`")]
    UnknownPurpose { value: String },
    #[error("unknown order unit `{value}`")]
    UnknownUnit { value: String },
    #[error("unknown order status `{value}`")]
    UnknownOrderStatus { value: String },
    #[error("column `{column}` holds an invalid value `{value}`")]
    InvalidColumn { column: &'static str, value: String },
}

/// 一个进程内的数据库句柄。克隆出来的句柄共用同一条连接。
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
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
             CREATE TABLE IF NOT EXISTS batch_states (
                 strategy_id TEXT NOT NULL,
                 batch_index INTEGER NOT NULL,
                 filled_units INTEGER NOT NULL,
                 finished INTEGER NOT NULL,
                 PRIMARY KEY (strategy_id, batch_index)
             );
             CREATE TABLE IF NOT EXISTS cell_states (
                 strategy_id TEXT NOT NULL,
                 cell_index INTEGER NOT NULL,
                 enabled INTEGER NOT NULL,
                 to_buy_back_units INTEGER NOT NULL,
                 PRIMARY KEY (strategy_id, cell_index)
             );
             CREATE TABLE IF NOT EXISTS orders (
                 client_order_id TEXT PRIMARY KEY,
                 strategy_id TEXT NOT NULL,
                 unit_kind TEXT NOT NULL,
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
             CREATE INDEX IF NOT EXISTS fills_by_strategy ON fills (strategy_id, filled_at);",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().expect("store lock poisoned")
    }

    /// 建策略：策略记录和初始格子状态写在同一个事务里。
    ///
    /// 分开写就可能留下「策略已存在、格子状态是空的」的记录，这种策略既跑不起来
    /// 也暂停不了（每次读状态都会因为格数对不上而失败）。
    pub fn insert_strategy(
        &self,
        strategy: &StoredStrategy,
        state: &StrategyState,
    ) -> Result<(), StoreError> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
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
        write_state(&transaction, &strategy.strategy_id, state)?;
        transaction.commit()?;
        Ok(())
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

    /// 读一个策略的状态：各批主仓的成交与结束标记、各格子的启用标记与待买回量。
    pub fn load_state(&self, strategy_id: &StrategyId) -> Result<StrategyState, StoreError> {
        let connection = self.lock();
        let mut batches = Vec::new();
        {
            let mut statement = connection.prepare(
                "SELECT filled_units, finished FROM batch_states
                 WHERE strategy_id = ?1 ORDER BY batch_index",
            )?;
            let rows = statement.query_map(params![strategy_id.as_str()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (filled, finished) = row?;
                batches.push(BatchState {
                    filled: Quantity::from_units(filled),
                    finished: finished != 0,
                });
            }
        }

        let mut cells = Vec::new();
        {
            let mut statement = connection.prepare(
                "SELECT enabled, to_buy_back_units FROM cell_states
                 WHERE strategy_id = ?1 ORDER BY cell_index",
            )?;
            let rows = statement.query_map(params![strategy_id.as_str()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (enabled, to_buy_back) = row?;
                cells.push(CellState {
                    enabled: enabled != 0,
                    to_buy_back: Quantity::from_units(to_buy_back),
                });
            }
        }
        Ok(StrategyState::new(batches, cells))
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
        let (unit_kind, unit_index) = unit_columns(record.unit);
        self.lock().execute(
            "INSERT INTO orders (
                 client_order_id, strategy_id, unit_kind, unit_index, purpose, limit_price_units,
                 quantity_units, cumulative_filled_units, reduce_only, status,
                 fills_reconciled, exchange_order_id, expires_at, placed_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                record.client_order_id.as_str(),
                strategy_id.as_str(),
                unit_kind,
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

    pub fn update_order(
        &self,
        strategy_id: &StrategyId,
        order: &StoredOrder,
    ) -> Result<(), StoreError> {
        let record = &order.record;
        self.lock().execute(
            "UPDATE orders SET
                 cumulative_filled_units = ?3, status = ?4, exchange_order_id = ?5,
                 fills_reconciled = ?6, updated_at = ?7
             WHERE client_order_id = ?1 AND strategy_id = ?2",
            params![
                record.client_order_id.as_str(),
                strategy_id.as_str(),
                record.cumulative_filled.units(),
                record.status.as_str(),
                order.exchange_order_id,
                i64::from(order.fills_reconciled),
                format_time(order.updated_at),
            ],
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

    /// 按挂单先后返回一个策略的所有订单。
    pub fn load_orders(&self, strategy_id: &StrategyId) -> Result<Vec<StoredOrder>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT client_order_id, unit_kind, unit_index, purpose, limit_price_units,
                    quantity_units, cumulative_filled_units, reduce_only, status,
                    fills_reconciled, exchange_order_id, expires_at, placed_at, updated_at
             FROM orders WHERE strategy_id = ?1 ORDER BY placed_at, rowid",
        )?;
        let rows = statement.query_map(params![strategy_id.as_str()], |row| {
            Ok(StoredOrderRow {
                client_order_id: row.get(0)?,
                unit_kind: row.get(1)?,
                unit_index: row.get(2)?,
                purpose: row.get(3)?,
                limit_price_units: row.get(4)?,
                quantity_units: row.get(5)?,
                cumulative_filled_units: row.get(6)?,
                reduce_only: row.get(7)?,
                status: row.get(8)?,
                fills_reconciled: row.get(9)?,
                exchange_order_id: row.get(10)?,
                expires_at: row.get(11)?,
                placed_at: row.get(12)?,
                updated_at: row.get(13)?,
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
                unit: parse_unit(&self.unit_kind, self.unit_index)?,
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
        "core_buy" => Ok(OrderPurpose::CoreBuy),
        "grid_buy" => Ok(OrderPurpose::GridBuy),
        "grid_sell" => Ok(OrderPurpose::GridSell),
        other => Err(StoreError::UnknownPurpose {
            value: other.to_string(),
        }),
    }
}

/// 订单归属单位存成两列：种类和下标。
fn unit_columns(unit: Unit) -> (&'static str, i64) {
    match unit {
        Unit::Batch(BatchIndex(index)) => ("batch", index as i64),
        Unit::Cell(CellIndex(index)) => ("cell", index as i64),
    }
}

fn parse_unit(kind: &str, index: i64) -> Result<Unit, StoreError> {
    let unknown = || StoreError::UnknownUnit {
        value: kind.to_string(),
    };
    let index = usize::try_from(index).map_err(|_| unknown())?;
    match kind {
        "batch" => Ok(Unit::Batch(BatchIndex(index))),
        "cell" => Ok(Unit::Cell(CellIndex(index))),
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
        "unknown" => Ok(OrderStatus::Unknown),
        other => Err(StoreError::UnknownOrderStatus {
            value: other.to_string(),
        }),
    }
}

/// 全量改写一个策略的状态。
///
/// 批次和格子都是整体替换的：它们的数量由配置决定，不会中途增减。
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
            "INSERT INTO batch_states (strategy_id, batch_index, filled_units, finished)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (index, batch) in state.batches().iter().enumerate() {
            statement.execute(params![
                strategy_id.as_str(),
                index as i64,
                batch.filled.units(),
                i64::from(batch.finished),
            ])?;
        }
    }

    transaction.execute(
        "DELETE FROM cell_states WHERE strategy_id = ?1",
        params![strategy_id.as_str()],
    )?;
    let mut statement = transaction.prepare(
        "INSERT INTO cell_states (strategy_id, cell_index, enabled, to_buy_back_units)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (index, cell) in state.cells().iter().enumerate() {
        statement.execute(params![
            strategy_id.as_str(),
            index as i64,
            i64::from(cell.enabled),
            cell.to_buy_back.units(),
        ])?;
    }
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
    use super::{Store, StoreError, StoredFill, StoredOrder, StrategyId, StrategyStatus};
    use chrono::{DateTime, TimeZone, Utc};
    use poise_strategy::batch::BatchIndex;
    use poise_strategy::grid::CellIndex;
    use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Unit};
    use poise_strategy::state::{BatchState, CellState, StrategyState};
    use poise_strategy::units::{Price, Quantity};

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap() + chrono::Duration::seconds(seconds)
    }

    fn order(cloid: &str) -> StoredOrder {
        StoredOrder {
            record: OrderRecord {
                client_order_id: ClientOrderId::try_new(cloid).unwrap(),
                unit: Unit::Cell(CellIndex(0)),
                purpose: OrderPurpose::GridBuy,
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
        assert!(StrategyStatus::parse("stopped").is_err());
    }

    #[test]
    fn an_order_keeps_the_unit_it_belongs_to() {
        let strategy_id = StrategyId::try_from("s-test".to_string()).unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut batch_order = order("0x02");
        batch_order.record.unit = Unit::Batch(BatchIndex(3));
        batch_order.record.purpose = OrderPurpose::CoreBuy;

        store.insert_order(&strategy_id, &order("0x01")).unwrap();
        store.insert_order(&strategy_id, &batch_order).unwrap();

        let stored = store.load_orders(&strategy_id).unwrap();
        assert_eq!(stored[0].record.unit, Unit::Cell(CellIndex(0)));
        assert_eq!(stored[1].record.unit, Unit::Batch(BatchIndex(3)));
        assert_eq!(stored[1].record.purpose, OrderPurpose::CoreBuy);
    }

    #[test]
    fn batch_and_cell_state_survive_reopening() {
        let strategy_id = StrategyId::try_from("s-test".to_string()).unwrap();
        let state = StrategyState::new(
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
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("poise.sqlite");
        let store = Store::open(&path).unwrap();
        store.save_applied_fills(&strategy_id, &state, &[]).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.load_state(&strategy_id).unwrap(), state);
    }
}
