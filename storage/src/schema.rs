use anyhow::{Result, ensure};
use rusqlite::Connection;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        // `persisted_track_presence` is a read-model helper for listing tracks
        // and updated-at metadata. Startup correctness must use the explicit
        // control state; PNL stats are rebuilt from track_pnl_records.
        "CREATE TABLE IF NOT EXISTS persisted_track_presence (
            track_id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS track_control_state (
            track_id TEXT PRIMARY KEY,
            control_state_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS track_pnl_records (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id TEXT NOT NULL,
            venue TEXT NOT NULL,
            symbol TEXT NOT NULL,
            occurred_at TEXT NOT NULL,
            kind TEXT NOT NULL,
            pnl_asset TEXT NOT NULL,
            source TEXT NOT NULL,
            source_key TEXT,
            order_id TEXT,
            trade_id TEXT,
            side TEXT,
            price REAL,
            qty REAL,
            realized_pnl REAL NOT NULL DEFAULT 0,
            trading_fee REAL NOT NULL DEFAULT 0,
            funding_fee REAL NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS track_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            track_id TEXT NOT NULL,
            event_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS track_effects (
            effect_id TEXT PRIMARY KEY,
            track_id TEXT NOT NULL,
            batch_id TEXT NOT NULL,
            sequence INTEGER NOT NULL,
            effect_json TEXT NOT NULL,
            status TEXT NOT NULL,
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS account_monitor_state (
            singleton_key INTEGER PRIMARY KEY CHECK (singleton_key = 1),
            trading_day TEXT NOT NULL,
            baseline_equity REAL NOT NULL,
            baseline_captured_at TEXT NOT NULL
        );",
    )?;

    ensure_table_schema(
        conn,
        "track_events",
        &["id", "track_id", "event_json", "created_at"],
        &["track_id", "event_json", "created_at"],
    )?;
    ensure_table_schema(
        conn,
        "persisted_track_presence",
        &["track_id", "created_at", "updated_at"],
        &["created_at", "updated_at"],
    )?;
    ensure_table_schema(
        conn,
        "track_control_state",
        &["track_id", "control_state_json", "updated_at"],
        &["control_state_json", "updated_at"],
    )?;
    ensure_table_schema(
        conn,
        "track_pnl_records",
        &[
            "id",
            "track_id",
            "venue",
            "symbol",
            "occurred_at",
            "kind",
            "pnl_asset",
            "source",
            "source_key",
            "order_id",
            "trade_id",
            "side",
            "price",
            "qty",
            "realized_pnl",
            "trading_fee",
            "funding_fee",
        ],
        &[
            "track_id",
            "venue",
            "symbol",
            "occurred_at",
            "kind",
            "pnl_asset",
            "source",
            "realized_pnl",
            "trading_fee",
            "funding_fee",
        ],
    )?;
    ensure_table_schema(
        conn,
        "track_effects",
        &[
            "effect_id",
            "track_id",
            "batch_id",
            "sequence",
            "effect_json",
            "status",
            "attempt_count",
            "last_error",
            "created_at",
            "updated_at",
        ],
        &[
            "track_id",
            "batch_id",
            "sequence",
            "effect_json",
            "status",
            "attempt_count",
            "created_at",
            "updated_at",
        ],
    )?;
    ensure_table_schema(
        conn,
        "account_monitor_state",
        &[
            "singleton_key",
            "trading_day",
            "baseline_equity",
            "baseline_captured_at",
        ],
        &["trading_day", "baseline_equity", "baseline_captured_at"],
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_track_events_created_at
         ON track_events(track_id, created_at);

         CREATE INDEX IF NOT EXISTS idx_track_effects_recent
         ON track_effects(track_id, updated_at DESC, created_at DESC, batch_id DESC, sequence DESC, effect_id DESC);

         CREATE INDEX IF NOT EXISTS idx_track_pnl_records_track_day
         ON track_pnl_records(track_id, occurred_at);

         CREATE UNIQUE INDEX IF NOT EXISTS idx_track_pnl_records_source_key
         ON track_pnl_records(source_key)
         WHERE source_key IS NOT NULL;",
    )?;
    Ok(())
}

fn ensure_table_schema(
    conn: &Connection,
    table: &str,
    expected_columns: &[&str],
    required_not_null_columns: &[&str],
) -> Result<()> {
    ensure_columns_exact(conn, table, expected_columns)?;
    ensure_columns_not_null(conn, table, required_not_null_columns)?;
    Ok(())
}

fn ensure_columns_exact(conn: &Connection, table: &str, expected: &[&str]) -> Result<()> {
    let columns = table_columns(conn, table)?;
    let expected = expected
        .iter()
        .map(|column| column.to_string())
        .collect::<Vec<_>>();

    ensure!(
        columns == expected,
        "sqlite schema for `{table}` columns do not match current schema: expected {expected:?}, found {columns:?}"
    );

    Ok(())
}

fn ensure_columns_not_null(conn: &Connection, table: &str, required: &[&str]) -> Result<()> {
    let columns = table_not_null_columns(conn, table)?;

    for column in required {
        ensure!(
            columns.iter().any(|existing| existing == column),
            "sqlite schema for `{table}` column `{column}` allows NULL"
        );
    }

    Ok(())
}

pub(crate) fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

fn table_not_null_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let columns = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let not_null = row.get::<_, i64>(3)? != 0;
            Ok((name, not_null))
        })?
        .filter_map(|column| match column {
            Ok((name, true)) => Some(Ok(name)),
            Ok((_name, false)) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn initialize_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();

        let events_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='track_events'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events_count, 1);

        let effects_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='track_effects'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(effects_count, 1);

        let account_monitor_state_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='account_monitor_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(account_monitor_state_count, 1);

        let control_state_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='track_control_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(control_state_count, 1);

        let removed_ledger_state_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='track_ledger_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(removed_ledger_state_count, 0);

        let pnl_records_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='track_pnl_records'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pnl_records_count, 1);
        assert!(
            table_columns(&conn, "track_pnl_records")
                .unwrap()
                .contains(&"pnl_asset".to_string())
        );

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_track_events_created_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);

        let pending_effects_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_track_effects_pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending_effects_index_count, 0);

        let effects_batch_sequence_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_track_effects_batch_sequence'",
                [],
                |row| row.get(0),
        )
        .unwrap();
        assert_eq!(effects_batch_sequence_index_count, 0);

        let recent_effects_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_track_effects_recent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recent_effects_index_count, 1);

        let control_columns = table_columns(&conn, "track_control_state").unwrap();
        assert_eq!(
            control_columns,
            vec![
                "track_id".to_string(),
                "control_state_json".to_string(),
                "updated_at".to_string(),
            ]
        );
        let pnl_record_columns = table_columns(&conn, "track_pnl_records").unwrap();
        assert_eq!(
            pnl_record_columns,
            vec![
                "id".to_string(),
                "track_id".to_string(),
                "venue".to_string(),
                "symbol".to_string(),
                "occurred_at".to_string(),
                "kind".to_string(),
                "pnl_asset".to_string(),
                "source".to_string(),
                "source_key".to_string(),
                "order_id".to_string(),
                "trade_id".to_string(),
                "side".to_string(),
                "price".to_string(),
                "qty".to_string(),
                "realized_pnl".to_string(),
                "trading_fee".to_string(),
                "funding_fee".to_string(),
            ]
        );
        let account_monitor_state_columns = table_columns(&conn, "account_monitor_state").unwrap();
        assert_eq!(
            account_monitor_state_columns,
            vec![
                "singleton_key".to_string(),
                "trading_day".to_string(),
                "baseline_equity".to_string(),
                "baseline_captured_at".to_string(),
            ]
        );
    }

    #[test]
    fn initialize_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
    }

    #[test]
    fn initialize_rejects_nullable_track_pnl_asset_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_pnl_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                track_id TEXT NOT NULL,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                kind TEXT NOT NULL,
                pnl_asset TEXT,
                source TEXT NOT NULL,
                source_key TEXT,
                order_id TEXT,
                trade_id TEXT,
                side TEXT,
                price REAL,
                qty REAL,
                realized_pnl REAL NOT NULL DEFAULT 0,
                trading_fee REAL NOT NULL DEFAULT 0,
                funding_fee REAL NOT NULL DEFAULT 0
            );",
        )
        .unwrap();

        let error = initialize(&conn).expect_err("nullable pnl_asset column should fail");

        assert!(
            format!("{error:#}").contains("column `pnl_asset` allows NULL"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn initialize_rejects_extra_account_monitor_state_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE account_monitor_state (
                singleton_key INTEGER PRIMARY KEY CHECK (singleton_key = 1),
                trading_day TEXT NOT NULL,
                baseline_equity REAL NOT NULL,
                baseline_captured_at TEXT NOT NULL,
                extra_column TEXT
            );",
        )
        .unwrap();

        let error = initialize(&conn).expect_err("extra account monitor column should fail");

        assert!(
            format!("{error:#}")
                .contains("sqlite schema for `account_monitor_state` columns do not match"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn initialize_rejects_nullable_current_account_monitor_state_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE account_monitor_state (
                singleton_key INTEGER PRIMARY KEY CHECK (singleton_key = 1),
                trading_day TEXT NOT NULL,
                baseline_equity REAL,
                baseline_captured_at TEXT NOT NULL
            );",
        )
        .unwrap();

        let error =
            initialize(&conn).expect_err("nullable current account monitor column should fail");

        assert!(
            format!("{error:#}").contains("column `baseline_equity` allows NULL"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn initialize_adds_recent_track_effects_index_for_websocket_detail_query() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();

        let recent_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_track_effects_recent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recent_index_count, 1);

        let mut stmt = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT effect_id, track_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM track_effects
                 WHERE track_id = ?1
                 ORDER BY updated_at DESC, created_at DESC, batch_id DESC, sequence DESC, effect_id DESC
                 LIMIT ?2",
            )
            .unwrap();
        let plan_details = stmt
            .query_map(rusqlite::params!["btc-core", 20_i64], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(
            plan_details
                .iter()
                .any(|detail| detail.contains("idx_track_effects_recent")),
            "unexpected query plan: {plan_details:?}"
        );
        assert!(
            !plan_details
                .iter()
                .any(|detail| detail.contains("USE TEMP B-TREE FOR ORDER BY")),
            "unexpected query plan: {plan_details:?}"
        );
    }

    #[test]
    fn initialize_does_not_create_track_snapshots_table() {
        let conn = Connection::open_in_memory().unwrap();

        initialize(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'track_snapshots'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count, 0);
    }
}
