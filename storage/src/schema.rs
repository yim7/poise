use anyhow::{Result, ensure};
use rusqlite::Connection;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS grid_snapshots (
            grid_id TEXT PRIMARY KEY,
            venue TEXT NOT NULL,
            symbol TEXT NOT NULL,
            config_json TEXT NOT NULL,
            status TEXT NOT NULL,
            current_exposure REAL NOT NULL,
            target_exposure REAL,
            pending_order_json TEXT,
            replacement_gate_reason_json TEXT,
            realized_pnl_day TEXT,
            realized_pnl_today REAL NOT NULL DEFAULT 0,
            unrealized_pnl REAL NOT NULL DEFAULT 0,
            reference_price REAL,
            out_of_band_since TEXT,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS domain_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            grid_id TEXT NOT NULL,
            event_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS grid_effects (
            effect_id TEXT PRIMARY KEY,
            grid_id TEXT NOT NULL,
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

    add_column_if_missing(
        conn,
        "grid_snapshots",
        "replacement_gate_reason_json",
        "TEXT",
    )?;

    ensure_columns_present(
        &conn,
        "grid_snapshots",
        &[
            "grid_id",
            "venue",
            "symbol",
            "config_json",
            "status",
            "current_exposure",
            "target_exposure",
            "pending_order_json",
            "replacement_gate_reason_json",
            "realized_pnl_day",
            "realized_pnl_today",
            "unrealized_pnl",
            "reference_price",
            "out_of_band_since",
            "updated_at",
        ],
    )?;
    ensure_columns_present(&conn, "domain_events", &["grid_id"])?;
    ensure_columns_present(
        &conn,
        "grid_effects",
        &[
            "effect_id",
            "grid_id",
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
        "CREATE INDEX IF NOT EXISTS idx_events_grid
         ON domain_events(grid_id, created_at);

         CREATE INDEX IF NOT EXISTS idx_grid_effects_pending
         ON grid_effects(status, created_at, batch_id, sequence, effect_id);

         CREATE INDEX IF NOT EXISTS idx_grid_effects_batch_sequence
         ON grid_effects(grid_id, batch_id, sequence, status);",
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
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='grid_snapshots'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(snapshots_count, 1);

        let events_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='domain_events'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events_count, 1);

        let effects_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='grid_effects'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(effects_count, 1);

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_events_grid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);

        let effects_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_grid_effects_pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(effects_index_count, 1);

        let effects_batch_index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_grid_effects_batch_sequence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(effects_batch_index_count, 1);
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
            "CREATE TABLE grid_snapshots (
                grid_id TEXT PRIMARY KEY,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
                status TEXT NOT NULL,
                current_exposure REAL NOT NULL,
                target_exposure REAL,
                pending_order_json TEXT,
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

        let mut stmt = conn.prepare("PRAGMA table_info(grid_snapshots)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(columns.contains(&"replacement_gate_reason_json".to_string()));
    }

    #[test]
    fn initialize_rejects_legacy_snapshot_table_missing_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE grid_snapshots (
                grid_id TEXT PRIMARY KEY,
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
    }

    #[test]
    fn initialize_rejects_legacy_domain_events_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE domain_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL,
                event_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            INSERT INTO domain_events (instance_id, event_json, created_at)
            VALUES ('BTCUSDT', '{\"band_reentered\":{\"price\":99.0}}', '2026-03-25T00:00:00Z');",
        )
        .unwrap();

        assert!(initialize(&conn).is_err());
    }

    #[test]
    fn initialize_rejects_legacy_grid_effects_table_without_batch_sequence() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE grid_effects (
                effect_id TEXT PRIMARY KEY,
                grid_id TEXT NOT NULL,
                effect_json TEXT NOT NULL,
                status TEXT NOT NULL,
                attempt_count INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX idx_grid_effects_pending
            ON grid_effects(status, created_at, effect_id);

            CREATE INDEX idx_grid_effects_batch_sequence
            ON grid_effects(grid_id, status);",
        )
        .unwrap();

        assert!(initialize(&conn).is_err());
    }
}
