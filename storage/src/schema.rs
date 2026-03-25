use anyhow::Result;
use rusqlite::Connection;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_snapshots (
            id TEXT PRIMARY KEY,
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

    ensure_column(conn, "instance_snapshots", "target_exposure", "REAL")?;
    ensure_column(conn, "instance_snapshots", "pending_order_json", "TEXT")?;
    ensure_column(conn, "instance_snapshots", "realized_pnl_day", "TEXT")?;
    ensure_column(
        conn,
        "instance_snapshots",
        "realized_pnl_today",
        "REAL NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "instance_snapshots",
        "unrealized_pnl",
        "REAL NOT NULL DEFAULT 0",
    )?;
    ensure_column(conn, "instance_snapshots", "reference_price", "REAL")?;
    ensure_column(conn, "instance_snapshots", "out_of_band_since", "TEXT")?;
    ensure_column(conn, "domain_events", "grid_id", "TEXT")?;
    backfill_reference_price(conn)?;
    backfill_grid_id(conn)?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_events_grid
         ON domain_events(grid_id, created_at);",
    )?;

    Ok(())
}

fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .any(|existing| existing == column);

    if !exists {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}");
        conn.execute_batch(&sql)?;
    }

    Ok(())
}

fn backfill_reference_price(conn: &Connection) -> Result<()> {
    if has_column(conn, "instance_snapshots", "last_price")?
        && has_column(conn, "instance_snapshots", "reference_price")?
    {
        conn.execute_batch(
            "UPDATE instance_snapshots
             SET reference_price = COALESCE(reference_price, last_price)
             WHERE last_price IS NOT NULL",
        )?;
    }

    Ok(())
}

fn backfill_grid_id(conn: &Connection) -> Result<()> {
    if has_column(conn, "domain_events", "instance_id")?
        && has_column(conn, "domain_events", "grid_id")?
    {
        conn.execute_batch(
            "UPDATE domain_events
             SET grid_id = COALESCE(grid_id, instance_id)
             WHERE instance_id IS NOT NULL",
        )?;
    }

    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .any(|existing| existing == column);

    Ok(exists)
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
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='instance_snapshots'",
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
    fn initialize_upgrades_existing_snapshot_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE instance_snapshots (
                id TEXT PRIMARY KEY,
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

        let mut stmt = conn
            .prepare("PRAGMA table_info(instance_snapshots)")
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(columns.contains(&"target_exposure".to_string()));
        assert!(columns.contains(&"pending_order_json".to_string()));
        assert!(columns.contains(&"realized_pnl_day".to_string()));
        assert!(columns.contains(&"realized_pnl_today".to_string()));
        assert!(columns.contains(&"unrealized_pnl".to_string()));
        assert!(columns.contains(&"out_of_band_since".to_string()));
    }

    #[test]
    fn initialize_upgrades_legacy_domain_events_table_and_backfills_grid_id() {
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

        initialize(&conn).unwrap();

        let mut stmt = conn.prepare("PRAGMA table_info(domain_events)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert!(columns.contains(&"grid_id".to_string()));

        let grid_id: Option<String> = conn
            .query_row("SELECT grid_id FROM domain_events WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(grid_id.as_deref(), Some("BTCUSDT"));
    }
}
