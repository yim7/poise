use anyhow::Result;
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
        );",
    )?;

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_events_grid
         ON domain_events(grid_id, created_at);",
    )?;

    Ok(())
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

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_events_grid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
    }

    #[test]
    fn initialize_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap();
    }

    #[test]
    fn initialize_does_not_upgrade_existing_snapshot_table() {
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

        initialize(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(grid_snapshots)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(!columns.contains(&"target_exposure".to_string()));
        assert!(!columns.contains(&"pending_order_json".to_string()));
        assert!(!columns.contains(&"realized_pnl_day".to_string()));
        assert!(!columns.contains(&"realized_pnl_today".to_string()));
        assert!(!columns.contains(&"unrealized_pnl".to_string()));
        assert!(!columns.contains(&"out_of_band_since".to_string()));
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
}
