use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension};

const ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CONSTRAINT: &str =
    "account_monitor_state_snapshot_completeness";
const ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CHECK: &str = "((last_observed_equity IS NULL AND last_observed_available IS NULL AND \
      last_observed_unrealized_pnl IS NULL AND last_observed_at IS NULL) OR \
      (last_observed_equity IS NOT NULL AND last_observed_available IS NOT NULL AND \
      last_observed_unrealized_pnl IS NOT NULL AND last_observed_at IS NOT NULL))";

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS track_snapshots (
            track_id TEXT PRIMARY KEY,
            restore_revision TEXT,
            venue TEXT,
            symbol TEXT,
            config_json TEXT,
            status TEXT NOT NULL,
            current_exposure REAL NOT NULL,
            desired_exposure REAL,
            manual_target_override REAL,
            executor_state_json TEXT,
            replacement_gate_reason_json TEXT,
            ledger_state_json TEXT,
            realized_pnl_day TEXT,
            realized_pnl_today REAL NOT NULL DEFAULT 0,
            realized_pnl_cumulative REAL NOT NULL DEFAULT 0,
            unrealized_pnl REAL NOT NULL DEFAULT 0,
            reference_price REAL,
            out_of_band_since TEXT,
            last_tick_at TEXT,
            market_data_stale_since TEXT,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS persisted_track_presence (
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

    add_column_if_missing(conn, "track_snapshots", "desired_exposure", "REAL")?;
    add_column_if_missing(conn, "track_snapshots", "executor_state_json", "TEXT")?;
    add_column_if_missing(conn, "track_snapshots", "manual_target_override", "REAL")?;
    add_column_if_missing(
        conn,
        "track_snapshots",
        "replacement_gate_reason_json",
        "TEXT",
    )?;
    add_column_if_missing(conn, "track_snapshots", "ledger_state_json", "TEXT")?;
    add_column_if_missing(
        conn,
        "track_snapshots",
        "realized_pnl_cumulative",
        "REAL NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "track_snapshots", "last_tick_at", "TEXT")?;
    add_column_if_missing(conn, "track_snapshots", "market_data_stale_since", "TEXT")?;
    add_column_if_missing(conn, "track_snapshots", "restore_revision", "TEXT")?;

    ensure_columns_present(
        conn,
        "track_snapshots",
        &[
            "track_id",
            "restore_revision",
            "venue",
            "symbol",
            "config_json",
            "status",
            "current_exposure",
            "desired_exposure",
            "manual_target_override",
            "executor_state_json",
            "replacement_gate_reason_json",
            "ledger_state_json",
            "realized_pnl_day",
            "realized_pnl_today",
            "realized_pnl_cumulative",
            "unrealized_pnl",
            "reference_price",
            "out_of_band_since",
            "last_tick_at",
            "market_data_stale_since",
            "updated_at",
        ],
    )?;
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

fn ensure_account_monitor_state_snapshot_completeness_constraint(conn: &Connection) -> Result<()> {
    let table_sql = table_sql(conn, "account_monitor_state")?.unwrap_or_default();
    if table_sql.contains(ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CONSTRAINT) {
        return Ok(());
    }

    let replacement_table = "account_monitor_state__new";
    let migration = format!(
        "BEGIN IMMEDIATE;
         CREATE TABLE {replacement_table} (
             singleton_key INTEGER PRIMARY KEY CHECK (singleton_key = 1),
             trading_day TEXT NOT NULL,
             baseline_equity REAL NOT NULL,
             baseline_captured_at TEXT NOT NULL,
             last_observed_equity REAL,
             last_observed_available REAL,
             last_observed_unrealized_pnl REAL,
             last_observed_at TEXT,
             CONSTRAINT {ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CONSTRAINT}
             CHECK ({ACCOUNT_MONITOR_STATE_SNAPSHOT_COMPLETENESS_CHECK})
         );
         INSERT INTO {replacement_table} (
             singleton_key,
             trading_day,
             baseline_equity,
             baseline_captured_at,
             last_observed_equity,
             last_observed_available,
             last_observed_unrealized_pnl,
             last_observed_at
         )
         SELECT
             singleton_key,
             trading_day,
             baseline_equity,
             baseline_captured_at,
             last_observed_equity,
             last_observed_available,
             last_observed_unrealized_pnl,
             last_observed_at
         FROM account_monitor_state;
         DROP TABLE account_monitor_state;
         ALTER TABLE {replacement_table} RENAME TO account_monitor_state;
         COMMIT;"
    );

    if let Err(error) = conn.execute_batch(&migration) {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(anyhow::Error::new(error))
            .context("failed to migrate account_monitor_state snapshot completeness constraint");
    }

    Ok(())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    column_sql: &str,
) -> Result<()> {
    let columns = table_columns(conn, table)?;
    if columns.iter().any(|existing| existing == column) {
        return Ok(());
    }

    let statement = format!("ALTER TABLE {table} ADD COLUMN {column} {column_sql}");
    conn.execute(&statement, [])?;
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

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
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
        assert!(columns.contains(&"desired_exposure".to_string()));
        assert!(!columns.contains(&"pending_order_json".to_string()));
    }

    #[test]
    fn initialize_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
    }

    #[test]
    fn initialize_upgrades_snapshot_table_missing_only_replacement_gate_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
                status TEXT NOT NULL,
                current_exposure REAL NOT NULL,
                desired_exposure REAL,
                executor_state_json TEXT,
                realized_pnl_day TEXT,
                realized_pnl_today REAL NOT NULL DEFAULT 0,
                realized_pnl_cumulative REAL NOT NULL DEFAULT 0,
                unrealized_pnl REAL NOT NULL DEFAULT 0,
                reference_price REAL,
                out_of_band_since TEXT,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();

        initialize(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(track_snapshots)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(columns.contains(&"replacement_gate_reason_json".to_string()));
        assert!(columns.contains(&"desired_exposure".to_string()));
    }

    #[test]
    fn initialize_rejects_legacy_track_snapshots_table_without_required_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
                status TEXT NOT NULL,
                current_exposure REAL NOT NULL,
                reference_price REAL,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();

        assert!(initialize(&conn).is_err());

        let mut stmt = conn.prepare("PRAGMA table_info(track_snapshots)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(columns.contains(&"desired_exposure".to_string()));
        assert!(!columns.contains(&"pending_order_json".to_string()));
        assert!(!columns.contains(&"realized_pnl_day".to_string()));
        assert!(!columns.contains(&"realized_pnl_today".to_string()));
        assert!(columns.contains(&"realized_pnl_cumulative".to_string()));
        assert!(!columns.contains(&"unrealized_pnl".to_string()));
        assert!(!columns.contains(&"out_of_band_since".to_string()));
    }

    #[test]
    fn initialize_upgrades_snapshot_table_missing_realized_pnl_cumulative() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
                status TEXT NOT NULL,
                current_exposure REAL NOT NULL,
                desired_exposure REAL,
                realized_pnl_day TEXT,
                realized_pnl_today REAL NOT NULL DEFAULT 0,
                unrealized_pnl REAL NOT NULL DEFAULT 0,
                reference_price REAL,
                out_of_band_since TEXT,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();

        initialize(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(track_snapshots)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(columns.contains(&"realized_pnl_cumulative".to_string()));
    }

    #[test]
    fn initialize_rejects_legacy_track_events_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL,
                event_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            INSERT INTO track_events (instance_id, event_json, created_at)
            VALUES ('BTCUSDT', '{\"band_reentered\":{\"price\":99.0}}', '2026-03-25T00:00:00Z');",
        )
        .unwrap();

        assert!(initialize(&conn).is_err());
    }

    #[test]
    fn initialize_rejects_legacy_track_effects_table_without_batch_sequence() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_effects (
                effect_id TEXT PRIMARY KEY,
                track_id TEXT NOT NULL,
                effect_json TEXT NOT NULL,
                status TEXT NOT NULL,
                attempt_count INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX idx_track_effects_pending
            ON track_effects(status, created_at, effect_id);

            CREATE INDEX idx_track_effects_batch_sequence
            ON track_effects(track_id, status);",
        )
        .unwrap();

        assert!(initialize(&conn).is_err());
    }

    #[test]
    fn initialize_upgrades_account_monitor_state_table_to_enforce_snapshot_completeness() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE account_monitor_state (
                singleton_key INTEGER PRIMARY KEY CHECK (singleton_key = 1),
                trading_day TEXT NOT NULL,
                baseline_equity REAL NOT NULL,
                baseline_captured_at TEXT NOT NULL,
                last_observed_equity REAL,
                last_observed_available REAL,
                last_observed_unrealized_pnl REAL,
                last_observed_at TEXT
            );

            INSERT INTO account_monitor_state (
                singleton_key,
                trading_day,
                baseline_equity,
                baseline_captured_at,
                last_observed_equity,
                last_observed_available,
                last_observed_unrealized_pnl,
                last_observed_at
            ) VALUES (
                1,
                '2026-04-04',
                12500.5,
                '2026-04-04T00:01:02+00:00',
                12450.0,
                9800.0,
                -120.0,
                '2026-04-04T01:02:03+00:00'
            );",
        )
        .unwrap();

        initialize(&conn).unwrap();

        let error = conn
            .execute(
                "UPDATE account_monitor_state
                 SET last_observed_available = NULL
                 WHERE singleton_key = 1",
                [],
            )
            .expect_err("partial snapshot row should violate completeness constraint");

        assert!(
            error.to_string().contains("CHECK constraint failed"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn initialize_rejects_legacy_account_monitor_state_with_partial_snapshot_row() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE account_monitor_state (
                singleton_key INTEGER PRIMARY KEY CHECK (singleton_key = 1),
                trading_day TEXT NOT NULL,
                baseline_equity REAL NOT NULL,
                baseline_captured_at TEXT NOT NULL,
                last_observed_equity REAL,
                last_observed_available REAL,
                last_observed_unrealized_pnl REAL,
                last_observed_at TEXT
            );

            INSERT INTO account_monitor_state (
                singleton_key,
                trading_day,
                baseline_equity,
                baseline_captured_at,
                last_observed_equity,
                last_observed_available,
                last_observed_unrealized_pnl,
                last_observed_at
            ) VALUES (
                1,
                '2026-04-04',
                12500.5,
                '2026-04-04T00:01:02+00:00',
                12450.0,
                NULL,
                -120.0,
                '2026-04-04T01:02:03+00:00'
            );",
        )
        .unwrap();

        let error = initialize(&conn).expect_err("partial legacy snapshot row should be rejected");
        let rendered = format!("{error:#}");

        assert!(
            rendered.contains("account_monitor_state"),
            "unexpected error: {rendered}"
        );
    }
}
