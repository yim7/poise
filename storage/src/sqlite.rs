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
        serde_json::from_str(config_json).or_else(|primary_error| {
            serde_json::from_str::<LegacyGridConfig>(config_json)
                .map(Into::into)
                .map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(primary_error),
                    )
                })
        })
    }

    fn deserialize_grid_status(status_json: &str) -> rusqlite::Result<GridStatus> {
        serde_json::from_str(status_json).or_else(|primary_error| {
            serde_json::from_str::<LegacyGridStatus>(status_json)
                .map(Into::into)
                .map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(primary_error),
                    )
                })
        })
    }

    fn deserialize_domain_event(event_json: &str) -> rusqlite::Result<DomainEvent> {
        serde_json::from_str(event_json).or_else(|primary_error| {
            serde_json::from_str::<LegacyDomainEvent>(event_json)
                .map(Into::into)
                .map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(primary_error),
                    )
                })
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

#[derive(serde::Deserialize)]
struct LegacyGridConfig {
    lower_price: f64,
    upper_price: f64,
    long_capacity: f64,
    short_capacity: f64,
    capacity_notional: f64,
    shape_family: LegacyShapeFamily,
    out_of_band_policy: LegacyOutOfBandPolicy,
}

impl From<LegacyGridConfig> for GridConfig {
    fn from(value: LegacyGridConfig) -> Self {
        Self {
            lower_price: value.lower_price,
            upper_price: value.upper_price,
            long_exposure_units: value.long_capacity,
            short_exposure_units: value.short_capacity,
            notional_per_unit: value.capacity_notional,
            shape_family: value.shape_family.into(),
            out_of_band_policy: value.out_of_band_policy.into(),
        }
    }
}

#[derive(serde::Deserialize)]
enum LegacyShapeFamily {
    Linear,
    Convex,
    Concave,
}

impl From<LegacyShapeFamily> for grid_core::strategy::ShapeFamily {
    fn from(value: LegacyShapeFamily) -> Self {
        match value {
            LegacyShapeFamily::Linear => Self::Linear,
            LegacyShapeFamily::Convex => Self::Convex,
            LegacyShapeFamily::Concave => Self::Concave,
        }
    }
}

#[derive(serde::Deserialize)]
enum LegacyOutOfBandPolicy {
    Freeze,
    ReduceOnly,
    Terminate,
    Hold,
}

impl From<LegacyOutOfBandPolicy> for grid_core::strategy::OutOfBandPolicy {
    fn from(value: LegacyOutOfBandPolicy) -> Self {
        match value {
            LegacyOutOfBandPolicy::Freeze => Self::Freeze,
            LegacyOutOfBandPolicy::ReduceOnly => Self::ReduceOnly,
            LegacyOutOfBandPolicy::Terminate => Self::Terminate,
            LegacyOutOfBandPolicy::Hold => Self::Hold,
        }
    }
}

#[derive(serde::Deserialize)]
enum LegacyBandBoundary {
    Below,
    Above,
}

impl From<LegacyBandBoundary> for grid_core::strategy::BandBoundary {
    fn from(value: LegacyBandBoundary) -> Self {
        match value {
            LegacyBandBoundary::Below => Self::Below,
            LegacyBandBoundary::Above => Self::Above,
        }
    }
}

#[derive(serde::Deserialize)]
enum LegacyGridStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}

impl From<LegacyGridStatus> for GridStatus {
    fn from(value: LegacyGridStatus) -> Self {
        match value {
            LegacyGridStatus::WaitingMarketData => Self::WaitingMarketData,
            LegacyGridStatus::Active => Self::Active,
            LegacyGridStatus::Frozen => Self::Frozen,
            LegacyGridStatus::ReducingOnly => Self::ReducingOnly,
            LegacyGridStatus::Holding => Self::Holding,
            LegacyGridStatus::Terminated => Self::Terminated,
            LegacyGridStatus::Paused => Self::Paused,
        }
    }
}

#[derive(serde::Deserialize)]
enum LegacyDomainEvent {
    ExposureTargetChanged { from: Exposure, to: Exposure },
    BandBreached {
        boundary: LegacyBandBoundary,
        price: f64,
    },
    BandReentered { price: f64 },
    PolicyTriggered { policy: LegacyOutOfBandPolicy },
    RiskCapApplied {
        intended: Exposure,
        capped: Exposure,
    },
    RiskDenied { reason: String },
}

impl From<LegacyDomainEvent> for DomainEvent {
    fn from(value: LegacyDomainEvent) -> Self {
        match value {
            LegacyDomainEvent::ExposureTargetChanged { from, to } => {
                Self::ExposureTargetChanged { from, to }
            }
            LegacyDomainEvent::BandBreached { boundary, price } => Self::BandBreached {
                boundary: boundary.into(),
                price,
            },
            LegacyDomainEvent::BandReentered { price } => Self::BandReentered { price },
            LegacyDomainEvent::PolicyTriggered { policy } => Self::PolicyTriggered {
                policy: policy.into(),
            },
            LegacyDomainEvent::RiskCapApplied { intended, capped } => {
                Self::RiskCapApplied { intended, capped }
            }
            LegacyDomainEvent::RiskDenied { reason } => Self::RiskDenied { reason },
        }
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
    use grid_engine::ports::{GridSnapshot, StateRepositoryPort};
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
                status: "NEW".into(),
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

        let loaded = storage
            .load_grid_state("test-1")
            .await
            .unwrap()
            .unwrap();
        assert!((loaded.current_exposure.0 - 6.0).abs() < f64::EPSILON);
        assert_eq!(loaded.reference_price, Some(96.0));
    }

    #[tokio::test]
    async fn save_rejects_mismatched_snapshot_id() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        let result = storage.save_transition("different-id", &snapshot, &[]).await;

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
    async fn load_grid_state_accepts_real_legacy_snapshot_json() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE instance_snapshots (
                id TEXT PRIMARY KEY,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
                status TEXT NOT NULL,
                current_exposure REAL NOT NULL,
                last_price REAL,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO instance_snapshots (
                id,
                symbol,
                config_json,
                status,
                current_exposure,
                last_price,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
        let loaded = storage.load_grid_state("legacy-grid").await.unwrap().unwrap();

        assert_eq!(loaded.status, GridStatus::Active);
        assert_eq!(loaded.reference_price, Some(95.0));
        assert_eq!(loaded.config.long_exposure_units, 8.0);
        assert_eq!(loaded.config.short_exposure_units, 6.0);
        assert_eq!(loaded.config.notional_per_unit, 375.0);
    }

    #[tokio::test]
    async fn list_events_accepts_real_legacy_event_json() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE domain_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL,
                event_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            INSERT INTO domain_events (instance_id, event_json, created_at)
            VALUES (
                'BTCUSDT',
                '{\"BandBreached\":{\"boundary\":\"Above\",\"price\":120.0}}',
                '2026-03-25T00:00:00Z'
            );",
        )
        .unwrap();

        let storage = SqliteStorage::from_connection(conn).unwrap();
        let events = storage.list_events("BTCUSDT").await.unwrap();

        assert_eq!(
            events,
            vec![DomainEvent::BandBreached {
                boundary: BandBoundary::Above,
                price: 120.0,
            }]
        );
    }
}
