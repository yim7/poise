use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema;
use grid_core::events::DomainEvent;
use grid_core::strategy::GridConfig;
use grid_core::types::Exposure;
use grid_engine::instance::{GridStatus, RiskState};
use grid_engine::ports::{GridSnapshot, StateRepositoryPort};

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

    fn deserialize_grid_config(config_json: &str) -> rusqlite::Result<GridConfig> {
        serde_json::from_str(config_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err))
        })
    }

    fn deserialize_grid_status(status_json: &str) -> rusqlite::Result<GridStatus> {
        serde_json::from_str(status_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(err))
        })
    }

    fn deserialize_domain_event(event_json: &str) -> rusqlite::Result<DomainEvent> {
        serde_json::from_str(event_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
        })
    }

    fn save_transition_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
        state: GridSnapshot,
        events: Vec<DomainEvent>,
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
        let out_of_band_since = state.out_of_band_since.map(|value| value.to_rfc3339());
        let updated_at = Utc::now().to_rfc3339();

        let mut conn = Self::lock_connection(&conn)?;
        let tx = conn
            .transaction()
            .context("failed to start sqlite transition transaction")?;
        tx.execute(
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
                reference_price,
                out_of_band_since,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                state.reference_price,
                out_of_band_since,
                updated_at
            ],
        )
        .context("failed to save instance snapshot")?;

        for event in events {
            let event_json =
                serde_json::to_string(&event).context("failed to serialize domain event")?;
            tx.execute(
                "INSERT INTO domain_events (grid_id, event_json, created_at)
                 VALUES (?1, ?2, ?3)",
                params![id, event_json, updated_at],
            )
            .context("failed to save domain event")?;
        }

        tx.commit()
            .context("failed to commit sqlite transition transaction")?;

        Ok(())
    }

    fn load_grid_state_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
    ) -> Result<Option<GridSnapshot>> {
        let conn = Self::lock_connection(&conn)?;
        let snapshot = conn
            .query_row(
                "SELECT id, symbol, config_json, status, current_exposure, target_exposure,
                        pending_order_json, realized_pnl_day, realized_pnl_today,
                        unrealized_pnl, reference_price, out_of_band_since
                 FROM instance_snapshots
                 WHERE id = ?1",
                params![id],
                |row| {
                    let config_json: String = row.get(2)?;
                    let status_json: String = row.get(3)?;
                    let pending_order_json: Option<String> = row.get(6)?;
                    let realized_pnl_day: Option<String> = row.get(7)?;
                    let out_of_band_since: Option<String> = row.get(11)?;
                    let config = Self::deserialize_grid_config(&config_json)?;
                    let status = Self::deserialize_grid_status(&status_json)?;
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
                    let out_of_band_since = out_of_band_since
                        .map(|value| {
                            DateTime::parse_from_rfc3339(&value)
                                .map(|parsed| parsed.with_timezone(&Utc))
                                .map_err(|err| {
                                    rusqlite::Error::FromSqlConversionFailure(
                                        11,
                                        rusqlite::types::Type::Text,
                                        Box::new(err),
                                    )
                                })
                        })
                        .transpose()?;

                    Ok(GridSnapshot {
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
                        reference_price: row.get(10)?,
                        out_of_band_since,
                    })
                },
            )
            .optional()
            .context("failed to load grid snapshot")?;

        Ok(snapshot)
    }

    fn list_events_blocking(conn: Arc<Mutex<Connection>>, id: String) -> Result<Vec<DomainEvent>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT event_json
                 FROM domain_events
                 WHERE grid_id = ?1
                 ORDER BY created_at ASC, id ASC",
            )
            .context("failed to prepare domain event query")?;

        let events = stmt
            .query_map(params![id], |row| row.get::<_, String>(0))
            .context("failed to query domain events")?
            .map(|event_json| {
                let event_json = event_json?;
                Self::deserialize_domain_event(&event_json)
            })
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize domain events")?;

        Ok(events)
    }
}

#[async_trait]
impl StateRepositoryPort for SqliteStorage {
    async fn save_transition(
        &self,
        id: &str,
        state: &GridSnapshot,
        events: &[DomainEvent],
    ) -> Result<()> {
        ensure!(
            id == state.id,
            "snapshot id mismatch: key `{id}` does not match state.id `{}`",
            state.id
        );

        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();
        let state = state.clone();
        let events = events.to_vec();

        tokio::task::spawn_blocking(move || {
            Self::save_transition_blocking(conn, id, state, events)
        })
        .await
        .context("failed to join save_transition blocking task")??;

        Ok(())
    }

    async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();

        tokio::task::spawn_blocking(move || Self::load_grid_state_blocking(conn, id))
            .await
            .context("failed to join load_grid_state blocking task")?
    }

    async fn list_events(&self, id: &str) -> Result<Vec<DomainEvent>> {
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();

        tokio::task::spawn_blocking(move || Self::list_events_blocking(conn, id))
            .await
            .context("failed to join list_events blocking task")?
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

    use grid_core::events::DomainEvent;
    use grid_core::strategy::BandBoundary;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{Exposure, Side};
    use grid_engine::instance::{GridStatus, PendingOrder, RiskState};
    use grid_engine::ports::{GridSnapshot, OrderStatus, StateRepositoryPort};
    use rusqlite::Connection;

    fn test_snapshot() -> GridSnapshot {
        GridSnapshot {
            id: "test-1".into(),
            symbol: "BTCUSDT".into(),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            status: GridStatus::Active,
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
                status: OrderStatus::New,
            }),
            risk_state: RiskState {
                realized_pnl_day: Some(NaiveDate::from_ymd_opt(2026, 3, 24).unwrap()),
                realized_pnl_today: 12.5,
                unrealized_pnl: -3.0,
            },
            reference_price: Some(95.0),
            out_of_band_since: Some(
                DateTime::parse_from_rfc3339("2026-03-24T07:30:00+00:00")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
        }
    }

    fn test_event() -> DomainEvent {
        DomainEvent::BandBreached {
            boundary: BandBoundary::Above,
            price: 120.0,
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
            .save_transition("test-1", &snapshot, &[])
            .await
            .unwrap();
        let loaded = storage.load_grid_state("test-1").await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, "test-1");
        assert_eq!(loaded.symbol, "BTCUSDT");
        assert_eq!(loaded.status, GridStatus::Active);
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
        assert_eq!(loaded.reference_price, Some(95.0));
        assert_eq!(loaded.out_of_band_since, snapshot.out_of_band_since);
    }

    #[tokio::test]
    async fn save_transition_persists_snapshot_and_events_atomically() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition("test-1", &snapshot, &[test_event()])
            .await
            .unwrap();

        let loaded = storage.load_grid_state("test-1").await.unwrap().unwrap();
        let events = storage.list_events("test-1").await.unwrap();

        assert_eq!(loaded.id, "test-1");
        assert_eq!(events, vec![test_event()]);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let storage = SqliteStorage::in_memory().unwrap();
        let loaded = storage.load_grid_state("nonexistent").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn save_overwrites_existing() {
        let storage = SqliteStorage::in_memory().unwrap();
        let mut snapshot = test_snapshot();

        storage
            .save_transition("test-1", &snapshot, &[])
            .await
            .unwrap();

        snapshot.current_exposure = Exposure(6.0);
        snapshot.reference_price = Some(96.0);
        storage
            .save_transition("test-1", &snapshot, &[])
            .await
            .unwrap();

        let loaded = storage.load_grid_state("test-1").await.unwrap().unwrap();
        assert!((loaded.current_exposure.0 - 6.0).abs() < f64::EPSILON);
        assert_eq!(loaded.reference_price, Some(96.0));
    }

    #[tokio::test]
    async fn save_rejects_mismatched_snapshot_id() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        let result = storage
            .save_transition("different-id", &snapshot, &[])
            .await;

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
                .save_transition("test-1", &snapshot, &[])
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
            .save_transition("test-1", &snapshot, &[])
            .await
            .unwrap();

        drop(storage);

        let reopened = SqliteStorage::new(&db_path).unwrap();
        let loaded = reopened.load_grid_state("test-1").await.unwrap();
        assert!(loaded.is_some());

        drop(reopened);
        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn load_grid_state_rejects_legacy_snapshot_json() {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO instance_snapshots (
                id,
                symbol,
                config_json,
                status,
                current_exposure,
                reference_price,
                target_exposure,
                pending_order_json,
                realized_pnl_day,
                realized_pnl_today,
                unrealized_pnl,
                out_of_band_since,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, 0, 0, NULL, ?7)",
            params![
                "legacy-grid",
                "BTCUSDT",
                serde_json::json!({
                    "lower_price": 90.0,
                    "upper_price": 110.0,
                    "long_capacity": 8.0,
                    "short_capacity": 6.0,
                    "capacity_notional": 375.0,
                    "shape_family": "Linear",
                    "out_of_band_policy": "Freeze"
                })
                .to_string(),
                "\"Active\"",
                4.0,
                95.0,
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();

        let storage = SqliteStorage::from_connection(conn).unwrap();
        let result = storage.load_grid_state("legacy-grid").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_events_rejects_legacy_event_json() {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO domain_events (grid_id, event_json, created_at)
             VALUES (?1, ?2, ?3)",
            params![
                "BTCUSDT",
                "{\"BandBreached\":{\"boundary\":\"Above\",\"price\":120.0}}",
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();

        let storage = SqliteStorage::from_connection(conn).unwrap();
        let result = storage.list_events("BTCUSDT").await;
        assert!(result.is_err());
    }
}
