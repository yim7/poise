use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema;
use grid_core::types::Exposure;
use grid_engine::instance::RiskState;
use grid_engine::ports::{InstanceSnapshot, PersistencePort};

pub struct SqliteStorage {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStorage {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).context("failed to open sqlite database")?;
        Self::from_connection(conn)
    }

    pub fn in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().context("failed to open in-memory sqlite database")?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        schema::initialize(&conn).context("failed to initialize sqlite schema")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock_connection(conn: &Mutex<Connection>) -> Result<MutexGuard<'_, Connection>> {
        conn.lock()
            .map_err(|err| anyhow!("failed to lock sqlite connection: {err}"))
    }

    fn save_instance_state_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
        state: InstanceSnapshot,
    ) -> Result<()> {
        let config_json =
            serde_json::to_string(&state.config).context("failed to serialize grid config")?;
        let status_json =
            serde_json::to_string(&state.status).context("failed to serialize instance status")?;
        let pending_order_json = state
            .pending_order
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("failed to serialize pending order")?;
        let realized_pnl_day = state
            .risk_state
            .realized_pnl_day
            .map(|day| day.format("%F").to_string());
        let updated_at = Utc::now().to_rfc3339();

        let conn = Self::lock_connection(&conn)?;
        conn.execute(
            "INSERT OR REPLACE INTO instance_snapshots (
                id,
                symbol,
                config_json,
                status,
                current_exposure,
                target_exposure,
                pending_order_json,
                realized_pnl_day,
                realized_pnl_today,
                unrealized_pnl,
                last_price,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
                state.symbol,
                config_json,
                status_json,
                state.current_exposure.0,
                state.target_exposure.as_ref().map(|exposure| exposure.0),
                pending_order_json,
                realized_pnl_day,
                state.risk_state.realized_pnl_today,
                state.risk_state.unrealized_pnl,
                state.last_price,
                updated_at
            ],
        )
        .context("failed to save instance snapshot")?;

        Ok(())
    }

    fn load_instance_state_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
    ) -> Result<Option<InstanceSnapshot>> {
        let conn = Self::lock_connection(&conn)?;
        let snapshot = conn
            .query_row(
                "SELECT id, symbol, config_json, status, current_exposure, target_exposure,
                        pending_order_json, realized_pnl_day, realized_pnl_today,
                        unrealized_pnl, last_price
                 FROM instance_snapshots
                 WHERE id = ?1",
                params![id],
                |row| {
                    let config_json: String = row.get(2)?;
                    let status_json: String = row.get(3)?;
                    let pending_order_json: Option<String> = row.get(6)?;
                    let realized_pnl_day: Option<String> = row.get(7)?;
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
                    let pending_order = pending_order_json
                        .map(|json| {
                            serde_json::from_str(&json).map_err(|err| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    6,
                                    rusqlite::types::Type::Text,
                                    Box::new(err),
                                )
                            })
                        })
                        .transpose()?;
                    let realized_pnl_day = realized_pnl_day
                        .map(|value| {
                            NaiveDate::parse_from_str(&value, "%F").map_err(|err| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    7,
                                    rusqlite::types::Type::Text,
                                    Box::new(err),
                                )
                            })
                        })
                        .transpose()?;
                    let target_exposure = row.get::<_, Option<f64>>(5)?.map(Exposure);

                    Ok(InstanceSnapshot {
                        id: row.get(0)?,
                        symbol: row.get(1)?,
                        config,
                        status,
                        current_exposure: Exposure(row.get(4)?),
                        target_exposure,
                        pending_order,
                        risk_state: RiskState {
                            realized_pnl_day,
                            realized_pnl_today: row.get(8)?,
                            unrealized_pnl: row.get(9)?,
                        },
                        last_price: row.get(10)?,
                    })
                },
            )
            .optional()
            .context("failed to load instance snapshot")?;

        Ok(snapshot)
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

        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();
        let state = state.clone();

        tokio::task::spawn_blocking(move || Self::save_instance_state_blocking(conn, id, state))
            .await
            .context("failed to join save_instance_state blocking task")??;

        Ok(())
    }

    async fn load_instance_state(&self, id: &str) -> Result<Option<InstanceSnapshot>> {
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();

        tokio::task::spawn_blocking(move || Self::load_instance_state_blocking(conn, id))
            .await
            .context("failed to join load_instance_state blocking task")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use std::time::{SystemTime, UNIX_EPOCH};

    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{Exposure, Side};
    use grid_engine::instance::{InstanceStatus, PendingOrder, RiskState};
    use grid_engine::ports::{InstanceSnapshot, PersistencePort};
    use rusqlite::Connection;

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
            target_exposure: Some(Exposure(6.0)),
            pending_order: Some(PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: Exposure(6.0),
                status: "NEW".into(),
            }),
            risk_state: RiskState {
                realized_pnl_day: Some(NaiveDate::from_ymd_opt(2026, 3, 24).unwrap()),
                realized_pnl_today: 12.5,
                unrealized_pnl: -3.0,
            },
            last_price: Some(95.0),
        }
    }

    fn temp_db_path() -> std::path::PathBuf {
        static TEMP_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = TEMP_DB_COUNTER.fetch_add(1, Ordering::Relaxed);

        env::temp_dir().join(format!("grid-storage-{timestamp}-{counter}.db"))
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_instance_state("test-1", &snapshot)
            .await
            .unwrap();
        let loaded = storage.load_instance_state("test-1").await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, "test-1");
        assert_eq!(loaded.symbol, "BTCUSDT");
        assert_eq!(loaded.status, InstanceStatus::Active);
        assert_eq!(loaded.config, snapshot.config);
        assert!((loaded.current_exposure.0 - 4.0).abs() < f64::EPSILON);
        assert_eq!(loaded.target_exposure, Some(Exposure(6.0)));
        assert!(loaded.pending_order.is_some());
        assert_eq!(
            loaded.risk_state.realized_pnl_day,
            Some(NaiveDate::from_ymd_opt(2026, 3, 24).unwrap())
        );
        assert!((loaded.risk_state.realized_pnl_today - 12.5).abs() < f64::EPSILON);
        assert!((loaded.risk_state.unrealized_pnl + 3.0).abs() < f64::EPSILON);
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

        storage
            .save_instance_state("test-1", &snapshot)
            .await
            .unwrap();

        snapshot.current_exposure = Exposure(6.0);
        snapshot.last_price = Some(96.0);
        storage
            .save_instance_state("test-1", &snapshot)
            .await
            .unwrap();

        let loaded = storage
            .load_instance_state("test-1")
            .await
            .unwrap()
            .unwrap();
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

    #[tokio::test(flavor = "current_thread")]
    async fn save_does_not_block_async_scheduler_while_waiting_for_db_lock() {
        let db_path = temp_db_path();
        let storage = Arc::new(SqliteStorage::new(&db_path).unwrap());

        storage
            .conn
            .lock()
            .unwrap()
            .busy_timeout(Duration::from_millis(250))
            .unwrap();

        let (ready_tx, ready_rx) = mpsc::channel();
        let lock_db_path = db_path.clone();
        let lock_thread = std::thread::spawn(move || {
            let conn = Connection::open(lock_db_path).unwrap();
            conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            conn.execute_batch("COMMIT").unwrap();
        });

        ready_rx.recv().unwrap();

        let save_storage = Arc::clone(&storage);
        let snapshot = test_snapshot();
        let save_task = tokio::spawn(async move {
            save_storage
                .save_instance_state("test-1", &snapshot)
                .await
                .unwrap();
        });

        let start = Instant::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let elapsed = start.elapsed();

        save_task.await.unwrap();
        lock_thread.join().unwrap();
        let _ = fs::remove_file(db_path);

        assert!(
            elapsed < Duration::from_millis(80),
            "tokio scheduler was blocked for {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn new_initializes_file_backed_storage() {
        let db_path = temp_db_path();
        let storage = SqliteStorage::new(&db_path).unwrap();
        let snapshot = test_snapshot();

        storage
            .save_instance_state("test-1", &snapshot)
            .await
            .unwrap();

        drop(storage);

        let reopened = SqliteStorage::new(&db_path).unwrap();
        let loaded = reopened.load_instance_state("test-1").await.unwrap();
        assert!(loaded.is_some());

        drop(reopened);
        let _ = fs::remove_file(db_path);
    }
}
