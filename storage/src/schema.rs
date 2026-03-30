use anyhow::{Result, ensure};
use rusqlite::Connection;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS track_snapshots (
            track_id TEXT PRIMARY KEY,
            venue TEXT NOT NULL,
            symbol TEXT NOT NULL,
            config_json TEXT NOT NULL,
            status TEXT NOT NULL,
            current_exposure REAL NOT NULL,
            target_exposure REAL,
            manual_target_override REAL,
            executor_state_json TEXT,
            replacement_gate_reason_json TEXT,
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
        );",
    )?;

    add_column_if_missing(conn, "track_snapshots", "executor_state_json", "TEXT")?;
    add_column_if_missing(conn, "track_snapshots", "manual_target_override", "REAL")?;
    add_column_if_missing(
        conn,
        "track_snapshots",
        "replacement_gate_reason_json",
        "TEXT",
    )?;
    add_column_if_missing(
        conn,
        "track_snapshots",
        "realized_pnl_cumulative",
        "REAL NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "track_snapshots", "last_tick_at", "TEXT")?;
    add_column_if_missing(conn, "track_snapshots", "market_data_stale_since", "TEXT")?;

    ensure_columns_present(
        &conn,
        "track_snapshots",
        &[
            "track_id",
            "venue",
            "symbol",
            "config_json",
            "status",
            "current_exposure",
            "target_exposure",
            "manual_target_override",
            "executor_state_json",
            "replacement_gate_reason_json",
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
    ensure_columns_present(&conn, "track_events", &["track_id"])?;
    ensure_columns_present(
        &conn,
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

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_track_events_created_at
         ON track_events(track_id, created_at);

         CREATE INDEX IF NOT EXISTS idx_track_effects_pending
         ON track_effects(status, created_at, batch_id, sequence, effect_id);

         CREATE INDEX IF NOT EXISTS idx_track_effects_batch_sequence
         ON track_effects(track_id, batch_id, sequence, status);",
    )?;
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

        let mut stmt = conn.prepare("PRAGMA table_info(track_snapshots)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.contains(&"track_id".to_string()));
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
                target_exposure REAL,
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

        assert!(!columns.contains(&"target_exposure".to_string()));
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
                target_exposure REAL,
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
}
