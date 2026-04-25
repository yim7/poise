use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension};

const ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CONSTRAINT: &str =
    "account_monitor_state_snapshot_completeness";

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        // `persisted_track_presence` is a read-model helper for listing tracks
        // and updated-at metadata. Startup correctness must use the explicit
        // control and ledger truth tables instead.
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

        CREATE TABLE IF NOT EXISTS track_ledger_state (
            track_id TEXT PRIMARY KEY,
            ledger_state_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
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
            baseline_captured_at TEXT NOT NULL,
            last_observed_equity REAL,
            last_observed_available REAL,
            last_observed_unrealized_pnl REAL,
            last_observed_at TEXT,
            CONSTRAINT account_monitor_state_snapshot_completeness CHECK (
                (last_observed_equity IS NULL AND last_observed_available IS NULL AND last_observed_unrealized_pnl IS NULL AND last_observed_at IS NULL)
                OR
                (last_observed_equity IS NOT NULL AND last_observed_available IS NOT NULL AND last_observed_unrealized_pnl IS NOT NULL AND last_observed_at IS NOT NULL)
            )
        );",
    )?;

    ensure_columns_present(conn, "track_events", &["track_id"])?;
    ensure_columns_present(
        conn,
        "persisted_track_presence",
        &["track_id", "created_at", "updated_at"],
    )?;
    ensure_columns_present(
        conn,
        "track_control_state",
        &["track_id", "control_state_json", "updated_at"],
    )?;
    ensure_columns_present(
        conn,
        "track_ledger_state",
        &["track_id", "ledger_state_json", "updated_at"],
    )?;
    ensure_columns_present(
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
    )?;
    ensure_columns_present(
        conn,
        "account_monitor_state",
        &[
            "singleton_key",
            "trading_day",
            "baseline_equity",
            "baseline_captured_at",
            "last_observed_equity",
            "last_observed_available",
            "last_observed_unrealized_pnl",
            "last_observed_at",
        ],
    )?;
    ensure_account_monitor_state_snapshot_completeness_constraint(conn)?;

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_track_events_created_at
         ON track_events(track_id, created_at);

         CREATE INDEX IF NOT EXISTS idx_track_effects_pending
         ON track_effects(status, created_at, batch_id, sequence, effect_id);

         CREATE INDEX IF NOT EXISTS idx_track_effects_batch_sequence
         ON track_effects(track_id, batch_id, sequence, status);

         CREATE INDEX IF NOT EXISTS idx_track_effects_recent
         ON track_effects(track_id, updated_at DESC, created_at DESC, batch_id DESC, sequence DESC, effect_id DESC);",
    )?;
    Ok(())
}

fn ensure_account_monitor_state_snapshot_completeness_constraint(conn: &Connection) -> Result<()> {
    let table_sql = table_sql(conn, "account_monitor_state")?.unwrap_or_default();
    ensure!(
        table_sql.contains(ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CONSTRAINT),
        "sqlite schema for `account_monitor_state` is missing required constraint `{ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CONSTRAINT}`"
    );
    Ok(())
}

fn ensure_columns_present(conn: &Connection, table: &str, required: &[&str]) -> Result<()> {
    let columns = table_columns(conn, table)?;

    for column in required {
        ensure!(
            columns.iter().any(|existing| existing == column),
            "legacy sqlite schema for `{table}` is missing required column `{column}`"
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

fn table_sql(conn: &Connection, table: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT sql
         FROM sqlite_master
         WHERE type = 'table' AND name = ?1",
    )?;
    let sql = stmt.query_row([table], |row| row.get(0)).optional()?;
    Ok(sql)
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

        let ledger_state_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='track_ledger_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ledger_state_count, 1);

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_track_events_created_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);

        let effects_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_track_effects_pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(effects_index_count, 1);

        let effects_batch_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_track_effects_batch_sequence'",
                [],
                |row| row.get(0),
        )
        .unwrap();
        assert_eq!(effects_batch_index_count, 1);

        let control_columns = table_columns(&conn, "track_control_state").unwrap();
        assert_eq!(
            control_columns,
            vec![
                "track_id".to_string(),
                "control_state_json".to_string(),
                "updated_at".to_string(),
            ]
        );
        let ledger_columns = table_columns(&conn, "track_ledger_state").unwrap();
        assert_eq!(
            ledger_columns,
            vec![
                "track_id".to_string(),
                "ledger_state_json".to_string(),
                "updated_at".to_string(),
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
