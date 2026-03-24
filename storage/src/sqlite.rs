use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema;
use grid_core::types::Exposure;
use grid_engine::ports::{InstanceSnapshot, PersistencePort};

pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).context("failed to open sqlite database")?;
        Self::from_connection(conn)
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory sqlite database")?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        schema::initialize(&conn).context("failed to initialize sqlite schema")?;
        Ok(Self { conn: Mutex::new(conn) })
    }
}

#[async_trait]
impl PersistencePort for SqliteStorage {
    async fn save_instance_state(&self, id: &str, state: &InstanceSnapshot) -> Result<()> {
        ensure!(
            id == state.id,
            "snapshot id mismatch: key `{id}` does not match state.id `{}`",
            state.id
        );

        let config_json =
            serde_json::to_string(&state.config).context("failed to serialize grid config")?;
        let status_json =
            serde_json::to_string(&state.status).context("failed to serialize instance status")?;
        let updated_at = Utc::now().to_rfc3339();

        let conn = self.conn.lock().map_err(|err| anyhow!("failed to lock sqlite connection: {err}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO instance_snapshots (
                id,
                symbol,
                config_json,
                status,
                current_exposure,
                last_price,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                state.symbol,
                config_json,
                status_json,
                state.current_exposure.0,
                state.last_price,
                updated_at
            ],
        )
        .context("failed to save instance snapshot")?;

        Ok(())
    }

    async fn load_instance_state(&self, id: &str) -> Result<Option<InstanceSnapshot>> {
        let conn = self.conn.lock().map_err(|err| anyhow!("failed to lock sqlite connection: {err}"))?;
        let snapshot = conn
            .query_row(
                "SELECT id, symbol, config_json, status, current_exposure, last_price
                 FROM instance_snapshots
                 WHERE id = ?1",
                params![id],
                |row| {
                    let config_json: String = row.get(2)?;
                    let status_json: String = row.get(3)?;
                    let config = serde_json::from_str(&config_json).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(err),
                        )
                    })?;
                    let status = serde_json::from_str(&status_json).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(err),
                        )
                    })?;

                    Ok(InstanceSnapshot {
                        id: row.get(0)?,
                        symbol: row.get(1)?,
                        config,
                        status,
                        current_exposure: Exposure(row.get(4)?),
                        last_price: row.get(5)?,
                    })
                },
            )
            .optional()
            .context("failed to load instance snapshot")?;

        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::Exposure;
    use grid_engine::instance::InstanceStatus;
    use grid_engine::ports::{InstanceSnapshot, PersistencePort};

    fn test_snapshot() -> InstanceSnapshot {
        InstanceSnapshot {
            id: "test-1".into(),
            symbol: "BTCUSDT".into(),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            status: InstanceStatus::Active,
            current_exposure: Exposure(4.0),
            last_price: Some(95.0),
        }
    }

    fn temp_db_path() -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("grid-storage-{suffix}.db"))
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage.save_instance_state("test-1", &snapshot).await.unwrap();
        let loaded = storage.load_instance_state("test-1").await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, "test-1");
        assert_eq!(loaded.symbol, "BTCUSDT");
        assert_eq!(loaded.status, InstanceStatus::Active);
        assert_eq!(loaded.config, snapshot.config);
        assert!((loaded.current_exposure.0 - 4.0).abs() < f64::EPSILON);
        assert_eq!(loaded.last_price, Some(95.0));
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let storage = SqliteStorage::in_memory().unwrap();
        let loaded = storage.load_instance_state("nonexistent").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn save_overwrites_existing() {
        let storage = SqliteStorage::in_memory().unwrap();
        let mut snapshot = test_snapshot();

        storage.save_instance_state("test-1", &snapshot).await.unwrap();

        snapshot.current_exposure = Exposure(6.0);
        snapshot.last_price = Some(96.0);
        storage.save_instance_state("test-1", &snapshot).await.unwrap();

        let loaded = storage.load_instance_state("test-1").await.unwrap().unwrap();
        assert!((loaded.current_exposure.0 - 6.0).abs() < f64::EPSILON);
        assert_eq!(loaded.last_price, Some(96.0));
    }

    #[tokio::test]
    async fn save_rejects_mismatched_snapshot_id() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        let result = storage.save_instance_state("different-id", &snapshot).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn new_initializes_file_backed_storage() {
        let db_path = temp_db_path();
        let storage = SqliteStorage::new(&db_path).unwrap();
        let snapshot = test_snapshot();

        storage.save_instance_state("test-1", &snapshot).await.unwrap();

        drop(storage);

        let reopened = SqliteStorage::new(&db_path).unwrap();
        let loaded = reopened.load_instance_state("test-1").await.unwrap();
        assert!(loaded.is_some());

        drop(reopened);
        let _ = fs::remove_file(db_path);
    }
}
