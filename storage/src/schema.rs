use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension};

const ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CONSTRAINT: &str =
    "account_monitor_state_snapshot_completeness";
const TRACK_SNAPSHOTS_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS track_snapshots (
    track_id TEXT PRIMARY KEY,
    restore_revision TEXT,
    status TEXT NOT NULL,
    current_exposure REAL NOT NULL,
    desired_exposure REAL,
    manual_target_override REAL,
    executor_state_json TEXT,
    replacement_gate_reason_json TEXT,
    ledger_state_json TEXT,
    unrealized_pnl REAL NOT NULL DEFAULT 0,
    strategy_price REAL,
    strategy_price_status TEXT NOT NULL,
    mark_price REAL,
    best_bid REAL,
    best_ask REAL,
    out_of_band_since TEXT,
    last_tick_at TEXT,
    market_data_stale_since TEXT,
    updated_at TEXT NOT NULL
);";
const TRACK_SNAPSHOTS_REQUIRED_COLUMNS: &[&str] = &[
    "track_id",
    "restore_revision",
    "status",
    "current_exposure",
    "desired_exposure",
    "manual_target_override",
    "executor_state_json",
    "replacement_gate_reason_json",
    "ledger_state_json",
    "unrealized_pnl",
    "strategy_price",
    "strategy_price_status",
    "mark_price",
    "best_bid",
    "best_ask",
    "out_of_band_since",
    "last_tick_at",
    "market_data_stale_since",
    "updated_at",
];

pub fn initialize(conn: &Connection) -> Result<()> {
    initialize_track_snapshots(conn)?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS persisted_track_presence (
            track_id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL,
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

        CREATE TABLE IF NOT EXISTS follow_up_retirements (
            track_id TEXT NOT NULL,
            batch_id TEXT NOT NULL,
            blocked_sequence INTEGER NOT NULL,
            closed_order_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (track_id, batch_id, blocked_sequence, closed_order_id)
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

    ensure_columns_present(conn, "track_snapshots", TRACK_SNAPSHOTS_REQUIRED_COLUMNS)?;
    ensure_columns_absent(conn, "track_snapshots", &["reference_price"])?;
    ensure_columns_present(conn, "track_events", &["track_id"])?;
    ensure_columns_present(
        conn,
        "persisted_track_presence",
        &["track_id", "created_at", "updated_at"],
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
        "follow_up_retirements",
        &[
            "track_id",
            "batch_id",
            "blocked_sequence",
            "closed_order_id",
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

         CREATE INDEX IF NOT EXISTS idx_follow_up_retirements_track
         ON follow_up_retirements(track_id, updated_at, batch_id, blocked_sequence, closed_order_id);",
    )?;
    Ok(())
}

fn initialize_track_snapshots(conn: &Connection) -> Result<()> {
    if table_sql(conn, "track_snapshots")?.is_none() {
        conn.execute_batch(TRACK_SNAPSHOTS_CREATE_SQL)?;
        return Ok(());
    }

    let columns = table_columns(conn, "track_snapshots")?;
    let needs_migration = columns.iter().any(|column| column == "reference_price")
        || TRACK_SNAPSHOTS_REQUIRED_COLUMNS
            .iter()
            .any(|required| !columns.iter().any(|existing| existing == required));

    if needs_migration {
        migrate_track_snapshots(conn)?;
    }

    Ok(())
}

fn migrate_track_snapshots(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "ALTER TABLE track_snapshots RENAME TO track_snapshots_legacy;",
    )?;
    conn.execute_batch(TRACK_SNAPSHOTS_CREATE_SQL)?;
    conn.execute_batch(
        "INSERT INTO track_snapshots (
            track_id,
            restore_revision,
            status,
            current_exposure,
            desired_exposure,
            manual_target_override,
            executor_state_json,
            replacement_gate_reason_json,
            ledger_state_json,
            unrealized_pnl,
            strategy_price,
            strategy_price_status,
            mark_price,
            best_bid,
            best_ask,
            out_of_band_since,
            last_tick_at,
            market_data_stale_since,
            updated_at
        )
        SELECT
            track_id,
            restore_revision,
            status,
            current_exposure,
            desired_exposure,
            manual_target_override,
            executor_state_json,
            replacement_gate_reason_json,
            ledger_state_json,
            unrealized_pnl,
            NULL,
            'stale',
            NULL,
            NULL,
            NULL,
            out_of_band_since,
            last_tick_at,
            market_data_stale_since,
            updated_at
        FROM track_snapshots_legacy;
        DROP TABLE track_snapshots_legacy;",
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

fn ensure_columns_absent(conn: &Connection, table: &str, forbidden: &[&str]) -> Result<()> {
    let columns = table_columns(conn, table)?;

    for column in forbidden {
        ensure!(
            columns.iter().all(|existing| existing != column),
            "sqlite schema for `{table}` still contains removed column `{column}`"
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

        let snapshots_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='track_snapshots'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(snapshots_count, 1);

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

        let follow_up_retirements_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='follow_up_retirements'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(follow_up_retirements_count, 1);

        let account_monitor_state_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='account_monitor_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(account_monitor_state_count, 1);

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

        let follow_up_retirements_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_follow_up_retirements_track'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(follow_up_retirements_index_count, 1);

        let mut stmt = conn.prepare("PRAGMA table_info(track_snapshots)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.contains(&"track_id".to_string()));
        assert!(columns.contains(&"restore_revision".to_string()));
        assert!(columns.contains(&"desired_exposure".to_string()));
        assert!(!columns.contains(&"venue".to_string()));
        assert!(!columns.contains(&"symbol".to_string()));
        assert!(!columns.contains(&"config_json".to_string()));
        assert!(!columns.contains(&"realized_pnl_day".to_string()));
        assert!(!columns.contains(&"realized_pnl_today".to_string()));
        assert!(!columns.contains(&"realized_pnl_cumulative".to_string()));
        assert!(!columns.contains(&"pending_order_json".to_string()));
    }

    #[test]
    fn initialize_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
    }

    #[test]
    fn initialize_rejects_track_snapshots_table_missing_current_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                current_exposure REAL NOT NULL,
                unrealized_pnl REAL NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();

        let error = initialize(&conn).expect_err("schema without current columns should fail");
        assert!(
            error.to_string().contains("track_snapshots"),
            "unexpected error: {error:#}"
        );
    }
}
