use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::protocol::{
    CommandLinks, CommandRecord, OpenOrder, RecentFill, RiskEvent, RiskLevel, RuntimeSnapshot,
    SystemEvent,
};

const RECENT_COMMAND_WINDOW: usize = 24;

#[derive(Debug, Clone)]
pub struct PersistedRuntime {
    pub snapshot: RuntimeSnapshot,
    pub risk_events: Vec<RiskEvent>,
    pub system_events: Vec<SystemEvent>,
    pub last_sequence: u64,
}

impl PersistedRuntime {
    pub fn in_memory_bootstrap() -> Self {
        Self::bootstrap_with_message("Rust in-memory runtime bootstrapped.")
    }

    pub fn sqlite_bootstrap() -> Self {
        Self::bootstrap_with_message("Rust runtime bootstrapped with SQLite storage.")
    }

    fn bootstrap_with_message(message: &str) -> Self {
        let now = now_utc();
        Self {
            snapshot: RuntimeSnapshot::empty_bootstrap(),
            risk_events: vec![],
            system_events: vec![SystemEvent {
                level: "info".into(),
                source: "bootstrap".into(),
                message: message.into(),
                created_at: now,
            }],
            last_sequence: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SqliteStorage {
    path: PathBuf,
}

impl SqliteStorage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create sqlite parent dir {}", parent.display())
            })?;
        }
        let storage = Self { path };
        let mut connection = storage.open_connection()?;
        storage.initialize_schema(&mut connection)?;
        Ok(storage)
    }

    pub fn persist_runtime(&self, state: &PersistedRuntime) -> Result<()> {
        let mut connection = self.open_connection()?;
        let tx = connection
            .transaction()
            .context("failed to start sqlite persistence transaction")?;

        tx.execute(
            "INSERT OR REPLACE INTO runtime_snapshots (sequence, created_at, snapshot_json)
             VALUES (?1, ?2, ?3)",
            params![
                state.last_sequence,
                now_utc(),
                serde_json::to_string(&state.snapshot)
                    .context("failed to serialize runtime snapshot")?
            ],
        )
        .context("failed to persist runtime snapshot row")?;

        replace_open_orders(&tx, &state.snapshot.execution.open_orders)?;
        replace_recent_fills(&tx, &state.snapshot.execution.recent_fills)?;
        upsert_command_records(&tx, &state.snapshot.execution.recent_commands)?;
        replace_risk_events(&tx, &state.risk_events)?;
        replace_system_events(&tx, &state.system_events)?;

        tx.commit()
            .context("failed to commit sqlite persistence transaction")
    }

    pub fn load_runtime(&self) -> Result<Option<PersistedRuntime>> {
        let connection = self.open_connection()?;
        let Some((last_sequence, snapshot_json)) = connection
            .query_row(
                "SELECT sequence, snapshot_json
                 FROM runtime_snapshots
                 ORDER BY sequence DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .context("failed to query latest runtime snapshot")?
        else {
            return Ok(None);
        };

        let mut snapshot = serde_json::from_str::<RuntimeSnapshot>(&snapshot_json)
            .context("failed to decode persisted runtime snapshot")?;
        snapshot.execution.open_orders = load_open_orders(&connection)?;
        snapshot.execution.recent_fills = load_recent_fills(&connection)?;
        snapshot.execution.recent_commands =
            load_command_records(&connection, Some(RECENT_COMMAND_WINDOW))?;

        Ok(Some(PersistedRuntime {
            snapshot,
            risk_events: load_risk_events(&connection)?,
            system_events: load_system_events(&connection)?,
            last_sequence,
        }))
    }

    pub fn load_command_audit(&self) -> Result<Vec<CommandRecord>> {
        let connection = self.open_connection()?;
        load_command_records(&connection, None)
    }

    pub fn load_command_record(&self, command_id: &str) -> Result<Option<CommandRecord>> {
        let connection = self.open_connection()?;
        connection
            .query_row(
                "SELECT command_id, command_type, status, summary, requested_at, accepted_at,
                        finished_at, client_order_ids_json, order_ids_json, trade_ids_json
                 FROM commands
                 WHERE command_id = ?1",
                [command_id],
                command_record_from_row,
            )
            .optional()
            .context("failed to query command record by id")
    }

    fn open_connection(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open sqlite db {}", self.path.display()))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(3))
            .context("failed to configure sqlite busy timeout")?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;",
            )
            .context("failed to configure sqlite pragmas")?;
        Ok(connection)
    }

    fn initialize_schema(&self, connection: &mut Connection) -> Result<()> {
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS runtime_snapshots (
                    sequence INTEGER PRIMARY KEY,
                    created_at TEXT NOT NULL,
                    snapshot_json TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS commands (
                    command_id TEXT PRIMARY KEY,
                    list_index INTEGER NOT NULL,
                    command_type TEXT NOT NULL,
                    status TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    requested_at TEXT NOT NULL,
                    accepted_at TEXT,
                    finished_at TEXT,
                    client_order_ids_json TEXT NOT NULL DEFAULT '[]',
                    order_ids_json TEXT NOT NULL DEFAULT '[]',
                    trade_ids_json TEXT NOT NULL DEFAULT '[]'
                );

                CREATE TABLE IF NOT EXISTS open_orders (
                    order_id TEXT PRIMARY KEY,
                    list_index INTEGER NOT NULL,
                    client_order_id TEXT NOT NULL,
                    side TEXT NOT NULL,
                    price REAL NOT NULL,
                    qty REAL NOT NULL,
                    filled_qty REAL NOT NULL,
                    status TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS fills (
                    trade_id TEXT PRIMARY KEY,
                    list_index INTEGER NOT NULL,
                    order_id TEXT NOT NULL,
                    client_order_id TEXT,
                    side TEXT NOT NULL,
                    price REAL NOT NULL,
                    qty REAL NOT NULL,
                    fee REAL NOT NULL,
                    realized_pnl REAL NOT NULL,
                    event_time TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS risk_events (
                    list_index INTEGER PRIMARY KEY,
                    severity TEXT NOT NULL,
                    code TEXT NOT NULL,
                    message TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    acknowledged_at TEXT
                );

                CREATE TABLE IF NOT EXISTS system_events (
                    list_index INTEGER PRIMARY KEY,
                    level TEXT NOT NULL,
                    source TEXT NOT NULL,
                    message TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );",
            )
            .context("failed to initialize sqlite schema")?;
        ensure_column(
            connection,
            "commands",
            "client_order_ids_json",
            "TEXT NOT NULL DEFAULT '[]'",
        )?;
        ensure_column(
            connection,
            "commands",
            "order_ids_json",
            "TEXT NOT NULL DEFAULT '[]'",
        )?;
        ensure_column(
            connection,
            "commands",
            "trade_ids_json",
            "TEXT NOT NULL DEFAULT '[]'",
        )?;
        ensure_column(connection, "fills", "client_order_id", "TEXT")
    }
}

fn replace_open_orders(tx: &Transaction<'_>, orders: &[OpenOrder]) -> Result<()> {
    tx.execute("DELETE FROM open_orders", [])
        .context("failed to clear open_orders")?;
    let mut statement = tx
        .prepare(
            "INSERT INTO open_orders (
                order_id, list_index, client_order_id, side, price, qty, filled_qty, status,
                created_at, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .context("failed to prepare open_orders insert")?;
    for (index, order) in orders.iter().enumerate() {
        statement
            .execute(params![
                order.order_id,
                index as i64,
                order.client_order_id,
                order.side,
                order.price,
                order.qty,
                order.filled_qty,
                order.status,
                order.created_at,
                order.updated_at
            ])
            .context("failed to insert open order")?;
    }
    Ok(())
}

fn replace_recent_fills(tx: &Transaction<'_>, fills: &[RecentFill]) -> Result<()> {
    tx.execute("DELETE FROM fills", [])
        .context("failed to clear fills")?;
    let mut statement = tx
        .prepare(
            "INSERT INTO fills (
                trade_id, list_index, order_id, client_order_id, side, price, qty, fee,
                realized_pnl, event_time
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .context("failed to prepare fills insert")?;
    for (index, fill) in fills.iter().enumerate() {
        statement
            .execute(params![
                fill.trade_id,
                index as i64,
                fill.order_id,
                fill.client_order_id,
                fill.side,
                fill.price,
                fill.qty,
                fill.fee,
                fill.realized_pnl,
                fill.event_time
            ])
            .context("failed to insert recent fill")?;
    }
    Ok(())
}

fn upsert_command_records(tx: &Transaction<'_>, commands: &[CommandRecord]) -> Result<()> {
    let mut statement = tx
        .prepare(
            "INSERT INTO commands (
                command_id, list_index, command_type, status, summary, requested_at, accepted_at,
                finished_at, client_order_ids_json, order_ids_json, trade_ids_json
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(command_id) DO UPDATE SET
                list_index = excluded.list_index,
                command_type = excluded.command_type,
                status = excluded.status,
                summary = excluded.summary,
                requested_at = excluded.requested_at,
                accepted_at = excluded.accepted_at,
                finished_at = excluded.finished_at,
                client_order_ids_json = excluded.client_order_ids_json,
                order_ids_json = excluded.order_ids_json,
                trade_ids_json = excluded.trade_ids_json",
        )
        .context("failed to prepare commands insert")?;
    for (index, command) in commands.iter().enumerate() {
        statement
            .execute(params![
                command.command_id,
                index as i64,
                enum_to_text(&command.command)?,
                enum_to_text(&command.status)?,
                command.summary,
                command.requested_at,
                command.accepted_at,
                command.finished_at,
                string_list_to_text(&command.links.client_order_ids)?,
                string_list_to_text(&command.links.order_ids)?,
                string_list_to_text(&command.links.trade_ids)?
            ])
            .context("failed to insert command record")?;
    }
    Ok(())
}

fn replace_risk_events(tx: &Transaction<'_>, events: &[RiskEvent]) -> Result<()> {
    tx.execute("DELETE FROM risk_events", [])
        .context("failed to clear risk_events")?;
    let mut statement = tx
        .prepare(
            "INSERT INTO risk_events (
                list_index, severity, code, message, created_at, acknowledged_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .context("failed to prepare risk_events insert")?;
    for (index, event) in events.iter().enumerate() {
        statement
            .execute(params![
                index as i64,
                enum_to_text(&event.severity)?,
                event.code,
                event.message,
                event.created_at,
                event.acknowledged_at
            ])
            .context("failed to insert risk event")?;
    }
    Ok(())
}

fn replace_system_events(tx: &Transaction<'_>, events: &[SystemEvent]) -> Result<()> {
    tx.execute("DELETE FROM system_events", [])
        .context("failed to clear system_events")?;
    let mut statement = tx
        .prepare(
            "INSERT INTO system_events (list_index, level, source, message, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .context("failed to prepare system_events insert")?;
    for (index, event) in events.iter().enumerate() {
        statement
            .execute(params![
                index as i64,
                event.level,
                event.source,
                event.message,
                event.created_at
            ])
            .context("failed to insert system event")?;
    }
    Ok(())
}

fn load_open_orders(connection: &Connection) -> Result<Vec<OpenOrder>> {
    let mut statement = connection
        .prepare(
            "SELECT order_id, client_order_id, side, price, qty, filled_qty, status, created_at,
                    updated_at
             FROM open_orders
             ORDER BY list_index ASC",
        )
        .context("failed to prepare open_orders query")?;
    let rows = statement
        .query_map([], |row| {
            Ok(OpenOrder {
                order_id: row.get(0)?,
                client_order_id: row.get(1)?,
                side: row.get(2)?,
                price: row.get(3)?,
                qty: row.get(4)?,
                filled_qty: row.get(5)?,
                status: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })
        .context("failed to query open_orders")?;
    collect_rows(rows)
}

fn load_recent_fills(connection: &Connection) -> Result<Vec<RecentFill>> {
    let mut statement = connection
        .prepare(
            "SELECT trade_id, order_id, client_order_id, side, price, qty, fee, realized_pnl,
                    event_time
             FROM fills
             ORDER BY list_index ASC",
        )
        .context("failed to prepare fills query")?;
    let rows = statement
        .query_map([], |row| {
            Ok(RecentFill {
                trade_id: row.get(0)?,
                order_id: row.get(1)?,
                client_order_id: row.get(2)?,
                side: row.get(3)?,
                price: row.get(4)?,
                qty: row.get(5)?,
                fee: row.get(6)?,
                realized_pnl: row.get(7)?,
                event_time: row.get(8)?,
            })
        })
        .context("failed to query fills")?;
    collect_rows(rows)
}

fn load_command_records(
    connection: &Connection,
    limit: Option<usize>,
) -> Result<Vec<CommandRecord>> {
    let query = if limit.is_some() {
        "SELECT command_id, command_type, status, summary, requested_at, accepted_at,
                finished_at, client_order_ids_json, order_ids_json, trade_ids_json
         FROM commands
         ORDER BY COALESCE(finished_at, accepted_at, requested_at) DESC, rowid DESC
         LIMIT ?1"
    } else {
        "SELECT command_id, command_type, status, summary, requested_at, accepted_at,
                finished_at, client_order_ids_json, order_ids_json, trade_ids_json
         FROM commands
         ORDER BY COALESCE(finished_at, accepted_at, requested_at) DESC, rowid DESC"
    };
    let mut statement = connection
        .prepare(query)
        .context("failed to prepare commands query")?;

    if let Some(limit) = limit {
        let rows = statement
            .query_map([limit as i64], command_record_from_row)
            .context("failed to query commands")?;
        return collect_rows(rows);
    }

    let rows = statement
        .query_map([], command_record_from_row)
        .context("failed to query commands")?;
    collect_rows(rows)
}

fn load_risk_events(connection: &Connection) -> Result<Vec<RiskEvent>> {
    let mut statement = connection
        .prepare(
            "SELECT severity, code, message, created_at, acknowledged_at
             FROM risk_events
             ORDER BY list_index ASC",
        )
        .context("failed to prepare risk_events query")?;
    let rows = statement
        .query_map([], |row| {
            Ok(RiskEvent {
                severity: enum_from_text_sql(0, row.get::<_, String>(0)?)?,
                code: row.get(1)?,
                message: row.get(2)?,
                created_at: row.get(3)?,
                acknowledged_at: row.get(4)?,
            })
        })
        .context("failed to query risk_events")?;
    collect_rows(rows)
}

fn load_system_events(connection: &Connection) -> Result<Vec<SystemEvent>> {
    let mut statement = connection
        .prepare(
            "SELECT level, source, message, created_at
             FROM system_events
             ORDER BY list_index ASC",
        )
        .context("failed to prepare system_events query")?;
    let rows = statement
        .query_map([], |row| {
            Ok(SystemEvent {
                level: row.get(0)?,
                source: row.get(1)?,
                message: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .context("failed to query system_events")?;
    collect_rows(rows)
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect sqlite rows")
}

fn enum_to_text<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_value(value)
        .context("failed to encode enum as json value")?
        .as_str()
        .map(ToOwned::to_owned)
        .context("enum did not encode as string")
}

fn enum_from_text<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(value.into()))
        .context("failed to decode enum from sqlite text")
}

fn enum_from_text_sql<T: DeserializeOwned>(column: usize, value: String) -> rusqlite::Result<T> {
    enum_from_text(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error.to_string())),
        )
    })
}

fn command_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandRecord> {
    Ok(CommandRecord {
        command_id: row.get(0)?,
        command: enum_from_text_sql(1, row.get::<_, String>(1)?)?,
        status: enum_from_text_sql(2, row.get::<_, String>(2)?)?,
        summary: row.get(3)?,
        requested_at: row.get(4)?,
        accepted_at: row.get(5)?,
        finished_at: row.get(6)?,
        links: CommandLinks {
            client_order_ids: string_list_from_sql(7, row.get::<_, String>(7)?)?,
            order_ids: string_list_from_sql(8, row.get::<_, String>(8)?)?,
            trade_ids: string_list_from_sql(9, row.get::<_, String>(9)?)?,
        },
    })
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection
        .prepare(&pragma)
        .with_context(|| format!("failed to inspect sqlite table {table}"))?;
    let existing = statement
        .query_map([], |row| row.get::<_, String>(1))
        .with_context(|| format!("failed to inspect sqlite table {table} columns"))?;
    let existing = collect_rows(existing)?;
    if existing.iter().any(|name| name == column) {
        return Ok(());
    }

    connection
        .execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )
        .with_context(|| format!("failed to add sqlite column {table}.{column}"))?;
    Ok(())
}

fn string_list_to_text(values: &[String]) -> Result<String> {
    serde_json::to_string(values).context("failed to encode sqlite string list")
}

fn string_list_from_text(value: &str) -> Result<Vec<String>> {
    serde_json::from_str(value).context("failed to decode sqlite string list")
}

fn string_list_from_sql(column: usize, value: String) -> rusqlite::Result<Vec<String>> {
    string_list_from_text(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error.to_string())),
        )
    })
}

fn now_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}
