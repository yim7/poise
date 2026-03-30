use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema;
use grid_core::events::{DomainEvent, ReplacementGateReason};
use grid_core::strategy::GridConfig;
use grid_core::types::Exposure;
use grid_engine::grid::{GridId, Instrument, Venue};
use grid_engine::ports::{
    CommittedGridWrite, EffectStatus, EffectStatusUpdate, GridReadRepositoryPort,
    PersistedGridEffect, StateRepositoryPort, StoredDomainEvent, StoredGridSnapshot,
};
use grid_engine::runtime::{ExecutorState, GridStatus, RiskState};
use grid_engine::snapshot::{GridRuntimeSnapshot, ObservedState};
use grid_engine::transition::GridEffect;

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

    fn deserialize_venue(venue: &str) -> rusqlite::Result<Venue> {
        match venue {
            "binance" => Ok(Venue::Binance),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown venue `{other}`"),
                )),
            )),
        }
    }

    fn deserialize_domain_event(event_json: &str) -> rusqlite::Result<DomainEvent> {
        serde_json::from_str(event_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
        })
    }

    fn deserialize_grid_effect(effect_json: &str) -> rusqlite::Result<GridEffect> {
        serde_json::from_str(effect_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err))
        })
    }

    fn deserialize_effect_status(status: &str) -> rusqlite::Result<EffectStatus> {
        match status {
            "pending" => Ok(EffectStatus::Pending),
            "executing" => Ok(EffectStatus::Executing),
            "succeeded" => Ok(EffectStatus::Succeeded),
            "superseded" => Ok(EffectStatus::Superseded),
            "failed" => Ok(EffectStatus::Failed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown effect status `{other}`"),
                )),
            )),
        }
    }

    fn deserialize_timestamp(value: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(value)
            .map(|parsed| parsed.with_timezone(&Utc))
            .map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    column,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })
    }

    fn persisted_effect_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PersistedGridEffect> {
        let effect_id = row.get::<_, String>(0)?;
        let grid_id = GridId::new(row.get::<_, String>(1)?);
        let batch_id = row.get::<_, String>(2)?;
        let sequence = row.get::<_, i64>(3)?;
        let effect_json = row.get::<_, String>(4)?;
        let status_text = row.get::<_, String>(5)?;
        let attempt_count = row.get::<_, i64>(6)?;
        let created_at = row.get::<_, String>(8)?;
        let updated_at = row.get::<_, String>(9)?;

        Ok(PersistedGridEffect {
            effect_id,
            grid_id,
            batch_id,
            sequence: u32::try_from(sequence).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Integer,
                    Box::new(err),
                )
            })?,
            effect: Self::deserialize_grid_effect(&effect_json)?,
            status: Self::deserialize_effect_status(&status_text)?,
            attempt_count: u32::try_from(attempt_count).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Integer,
                    Box::new(err),
                )
            })?,
            last_error: row.get(7)?,
            created_at: Self::deserialize_timestamp(&created_at, 8)?,
            updated_at: Self::deserialize_timestamp(&updated_at, 9)?,
        })
    }

    fn save_transition_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
        state: GridRuntimeSnapshot,
        events: Vec<DomainEvent>,
        effects: Vec<GridEffect>,
        effect_status_update: Option<EffectStatusUpdate>,
    ) -> Result<CommittedGridWrite> {
        let config_json =
            serde_json::to_string(&state.config).context("failed to serialize grid config")?;
        let status_json =
            serde_json::to_string(&state.status).context("failed to serialize grid status")?;
        let executor_state_json = serde_json::to_string(&state.executor_state)
            .context("failed to serialize executor state")?;
        let replacement_gate_reason_json = state
            .replacement_gate_reason
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("failed to serialize replacement gate reason")?;
        let realized_pnl_day = state
            .risk
            .realized_pnl_day
            .map(|day| day.format("%F").to_string());
        let out_of_band_since = state
            .observed
            .out_of_band_since
            .map(|value| value.to_rfc3339());
        let updated_at = Utc::now();
        let updated_at_text = updated_at.to_rfc3339();
        let batch_nonce = updated_at
            .timestamp_nanos_opt()
            .unwrap_or(updated_at.timestamp_micros() * 1_000);
        let batch_id = batch_nonce.to_string();

        let mut conn = Self::lock_connection(&conn)?;
        let tx = conn
            .transaction()
            .context("failed to start sqlite transition transaction")?;
        tx.execute(
            "INSERT OR REPLACE INTO grid_snapshots (
                grid_id,
                venue,
                symbol,
                config_json,
                status,
                current_exposure,
                target_exposure,
                executor_state_json,
                replacement_gate_reason_json,
                realized_pnl_day,
                realized_pnl_today,
                realized_pnl_cumulative,
                unrealized_pnl,
                reference_price,
                out_of_band_since,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                id,
                state.instrument.venue.as_str(),
                state.instrument.symbol,
                config_json,
                status_json,
                state.current_exposure.0,
                state.target_exposure.as_ref().map(|exposure| exposure.0),
                executor_state_json,
                replacement_gate_reason_json,
                realized_pnl_day,
                state.risk.realized_pnl_today,
                state.risk.realized_pnl_cumulative,
                state.risk.unrealized_pnl,
                state.observed.reference_price,
                out_of_band_since,
                updated_at_text
            ],
        )
        .context("failed to save grid snapshot")?;

        for event in events {
            let event_json =
                serde_json::to_string(&event).context("failed to serialize domain event")?;
            tx.execute(
                "INSERT INTO domain_events (grid_id, event_json, created_at)
                 VALUES (?1, ?2, ?3)",
                params![id, event_json, updated_at_text],
            )
            .context("failed to save domain event")?;
        }

        let grid_id = GridId::new(id.clone());
        let mut persisted_effects = Vec::new();
        for (index, effect) in effects.into_iter().enumerate() {
            if matches!(effect, GridEffect::NoOp) {
                continue;
            }

            let effect_id = format!("{id}:{batch_nonce}:{index}");
            let sequence = u32::try_from(index).context("effect sequence overflow")?;
            let effect_json =
                serde_json::to_string(&effect).context("failed to serialize grid effect")?;
            tx.execute(
                "INSERT INTO grid_effects (
                    effect_id,
                    grid_id,
                    batch_id,
                    sequence,
                    effect_json,
                    status,
                    attempt_count,
                    last_error,
                    created_at,
                    updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    effect_id,
                    id,
                    batch_id.as_str(),
                    i64::from(sequence),
                    effect_json,
                    EffectStatus::Pending.as_str(),
                    0_i64,
                    Option::<String>::None,
                    updated_at_text,
                    updated_at_text
                ],
            )
            .context("failed to save grid effect")?;

            persisted_effects.push(PersistedGridEffect {
                effect_id,
                grid_id: grid_id.clone(),
                batch_id: batch_id.clone(),
                sequence,
                effect,
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: updated_at,
                updated_at,
            });
        }

        if let Some(effect_status_update) = effect_status_update {
            let changed = tx
                .execute(
                    "UPDATE grid_effects
                     SET status = ?1,
                         attempt_count = attempt_count + ?2,
                         last_error = ?3,
                         updated_at = ?4
                     WHERE effect_id = ?5",
                    params![
                        effect_status_update.status.as_str(),
                        i64::from(effect_status_update.attempt_delta),
                        effect_status_update.last_error,
                        updated_at_text,
                        effect_status_update.effect_id
                    ],
                )
                .context("failed to update grid effect status in transition transaction")?;
            ensure!(
                changed == 1,
                "effect status update affected {changed} rows in transition transaction"
            );
        }

        tx.commit()
            .context("failed to commit sqlite transition transaction")?;

        Ok(CommittedGridWrite {
            grid_id,
            effects: persisted_effects,
        })
    }

    fn load_grid_state_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
    ) -> Result<Option<GridRuntimeSnapshot>> {
        let conn = Self::lock_connection(&conn)?;
        let snapshot = conn
            .query_row(
                "SELECT grid_id, venue, symbol, config_json, status, current_exposure, target_exposure,
                        executor_state_json, replacement_gate_reason_json, realized_pnl_day,
                        realized_pnl_today, realized_pnl_cumulative, unrealized_pnl,
                        reference_price, out_of_band_since
                 FROM grid_snapshots
                 WHERE grid_id = ?1",
                params![id],
                Self::grid_snapshot_from_row,
            )
            .optional()
            .context("failed to load grid snapshot")?;

        Ok(snapshot)
    }

    fn load_grid_snapshot_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
    ) -> Result<Option<StoredGridSnapshot>> {
        let conn = Self::lock_connection(&conn)?;
        let snapshot = conn
            .query_row(
                "SELECT grid_id, venue, symbol, config_json, status, current_exposure, target_exposure,
                        executor_state_json, replacement_gate_reason_json, realized_pnl_day,
                        realized_pnl_today, realized_pnl_cumulative, unrealized_pnl,
                        reference_price, out_of_band_since, updated_at
                 FROM grid_snapshots
                 WHERE grid_id = ?1",
                params![id],
                Self::stored_grid_snapshot_from_row,
            )
            .optional()
            .context("failed to load grid snapshot record")?;

        Ok(snapshot)
    }

    fn grid_snapshot_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GridRuntimeSnapshot> {
        let venue: String = row.get(1)?;
        let config_json: String = row.get(3)?;
        let status_json: String = row.get(4)?;
        let executor_state_json: String = row.get(7)?;
        let replacement_gate_reason_json: Option<String> = row.get(8)?;
        let realized_pnl_day: Option<String> = row.get(9)?;
        let out_of_band_since: Option<String> = row.get(14)?;
        let config = Self::deserialize_grid_config(&config_json)?;
        let status = Self::deserialize_grid_status(&status_json)?;
        let venue = Self::deserialize_venue(&venue)?;
        let executor_state =
            serde_json::from_str::<ExecutorState>(&executor_state_json).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?;
        let replacement_gate_reason = replacement_gate_reason_json
            .map(|json| {
                serde_json::from_str::<ReplacementGateReason>(&json).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
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
                        9,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })
            })
            .transpose()?;
        let target_exposure = row.get::<_, Option<f64>>(6)?.map(Exposure);
        let out_of_band_since = out_of_band_since
            .map(|value| {
                DateTime::parse_from_rfc3339(&value)
                    .map(|parsed| parsed.with_timezone(&Utc))
                    .map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            14,
                            rusqlite::types::Type::Text,
                            Box::new(err),
                        )
                    })
            })
            .transpose()?;

        Ok(GridRuntimeSnapshot {
            grid_id: GridId::new(row.get::<_, String>(0)?),
            instrument: Instrument::new(venue, row.get::<_, String>(2)?),
            config,
            status,
            current_exposure: Exposure(row.get(5)?),
            target_exposure,
            executor_state,
            replacement_gate_reason,
            risk: RiskState {
                realized_pnl_day,
                realized_pnl_today: row.get(10)?,
                realized_pnl_cumulative: row.get(11)?,
                unrealized_pnl: row.get(12)?,
            },
            observed: ObservedState {
                reference_price: row.get(13)?,
                out_of_band_since,
            },
        })
    }

    fn stored_grid_snapshot_from_row(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<StoredGridSnapshot> {
        let updated_at: String = row.get(15)?;

        Ok(StoredGridSnapshot {
            snapshot: Self::grid_snapshot_from_row(row)?,
            updated_at: Self::deserialize_timestamp(&updated_at, 15)?,
        })
    }

    fn list_grid_snapshots_blocking(
        conn: Arc<Mutex<Connection>>,
    ) -> Result<Vec<StoredGridSnapshot>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT grid_id, venue, symbol, config_json, status, current_exposure, target_exposure,
                        executor_state_json, replacement_gate_reason_json, realized_pnl_day,
                        realized_pnl_today, realized_pnl_cumulative, unrealized_pnl,
                        reference_price, out_of_band_since, updated_at
                 FROM grid_snapshots
                 ORDER BY grid_id ASC",
            )
            .context("failed to prepare grid snapshot list query")?;

        let snapshots = stmt
            .query_map([], Self::stored_grid_snapshot_from_row)
            .context("failed to query grid snapshots")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize grid snapshots")?;
        Ok(snapshots)
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

    fn list_recent_grid_events_blocking(
        conn: Arc<Mutex<Connection>>,
        grid_id: GridId,
        limit: usize,
    ) -> Result<Vec<StoredDomainEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).context("event limit overflow")?;
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, grid_id, event_json, created_at
                 FROM domain_events
                 WHERE grid_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?2",
            )
            .context("failed to prepare recent domain event query")?;

        let mut events = stmt
            .query_map(params![grid_id.as_str(), limit], |row| {
                let event_json: String = row.get(2)?;
                let created_at: String = row.get(3)?;
                Ok(StoredDomainEvent {
                    id: row.get(0)?,
                    grid_id: GridId::new(row.get::<_, String>(1)?),
                    event: Self::deserialize_domain_event(&event_json)?,
                    created_at: Self::deserialize_timestamp(&created_at, 3)?,
                })
            })
            .context("failed to query recent domain events")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize recent domain events")?;
        events.reverse();
        Ok(events)
    }

    fn list_recent_grid_effects_blocking(
        conn: Arc<Mutex<Connection>>,
        grid_id: GridId,
        limit: usize,
    ) -> Result<Vec<PersistedGridEffect>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).context("effect limit overflow")?;
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT effect_id, grid_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM grid_effects
                 WHERE grid_id = ?1
                 ORDER BY updated_at DESC, created_at DESC, batch_id DESC, sequence DESC, effect_id DESC
                 LIMIT ?2",
            )
            .context("failed to prepare recent grid effect query")?;

        let mut effects = stmt
            .query_map(
                params![grid_id.as_str(), limit],
                Self::persisted_effect_from_row,
            )
            .context("failed to query recent grid effects")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize recent grid effects")?;
        effects.reverse();
        Ok(effects)
    }

    fn list_dispatchable_effects_blocking(
        conn: Arc<Mutex<Connection>>,
    ) -> Result<Vec<PersistedGridEffect>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT effect_id, grid_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM grid_effects ge
                 WHERE ge.status = ?1
                   AND NOT EXISTS (
                       SELECT 1
                       FROM grid_effects prior
                       WHERE prior.grid_id = ge.grid_id
                         AND prior.batch_id = ge.batch_id
                         AND prior.sequence < ge.sequence
                         AND prior.status NOT IN (?2, ?3)
                   )
                 ORDER BY ge.created_at ASC, ge.batch_id ASC, ge.sequence ASC, ge.effect_id ASC",
            )
            .context("failed to prepare pending effect query")?;

        let effects = stmt
            .query_map(
                params![
                    EffectStatus::Pending.as_str(),
                    EffectStatus::Succeeded.as_str(),
                    EffectStatus::Superseded.as_str()
                ],
                Self::persisted_effect_from_row,
            )
            .context("failed to query pending effects")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize pending effects")?;

        Ok(effects)
    }

    fn list_pending_submit_effects_for_grid_blocking(
        conn: Arc<Mutex<Connection>>,
        grid_id: GridId,
    ) -> Result<Vec<PersistedGridEffect>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT effect_id, grid_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM grid_effects ge
                 WHERE ge.grid_id = ?1
                   AND ge.status = ?2
                   AND NOT EXISTS (
                       SELECT 1
                       FROM grid_effects prior
                       WHERE prior.grid_id = ge.grid_id
                         AND prior.batch_id = ge.batch_id
                         AND prior.sequence < ge.sequence
                         AND prior.status NOT IN (?3, ?4)
                   )
                 ORDER BY ge.created_at ASC, ge.batch_id ASC, ge.sequence ASC, ge.effect_id ASC",
            )
            .context("failed to prepare grid-scoped pending submit effect query")?;

        let effects = stmt
            .query_map(
                params![
                    grid_id.as_str(),
                    EffectStatus::Pending.as_str(),
                    EffectStatus::Succeeded.as_str(),
                    EffectStatus::Superseded.as_str()
                ],
                Self::persisted_effect_from_row,
            )
            .context("failed to query grid-scoped pending submit effects")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize grid-scoped pending submit effects")?;

        Ok(effects
            .into_iter()
            .filter(|effect| matches!(effect.effect, GridEffect::SubmitOrder { .. }))
            .collect())
    }
}

#[async_trait]
impl StateRepositoryPort for SqliteStorage {
    async fn save_transition_with_effect_status(
        &self,
        id: &str,
        state: &GridRuntimeSnapshot,
        events: &[DomainEvent],
        effects: &[GridEffect],
        effect_status_update: Option<&EffectStatusUpdate>,
    ) -> Result<CommittedGridWrite> {
        ensure!(
            id == state.grid_id.as_str(),
            "snapshot id mismatch: key `{id}` does not match snapshot.grid_id `{}`",
            state.grid_id.as_str()
        );

        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();
        let state = state.clone();
        let events = events.to_vec();
        let effects = effects.to_vec();
        let effect_status_update = effect_status_update.cloned();

        tokio::task::spawn_blocking(move || {
            Self::save_transition_blocking(conn, id, state, events, effects, effect_status_update)
        })
        .await
        .context("failed to join save_transition_with_effect_status blocking task")?
    }

    async fn load_grid_state(&self, id: &str) -> Result<Option<GridRuntimeSnapshot>> {
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

    async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedGridEffect>> {
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || Self::list_dispatchable_effects_blocking(conn))
            .await
            .context("failed to join list_dispatchable_effects blocking task")?
    }

    async fn list_pending_submit_effects_for_grid(
        &self,
        grid_id: &GridId,
    ) -> Result<Vec<PersistedGridEffect>> {
        let conn = Arc::clone(&self.conn);
        let grid_id = grid_id.clone();

        tokio::task::spawn_blocking(move || {
            Self::list_pending_submit_effects_for_grid_blocking(conn, grid_id)
        })
        .await
        .context("failed to join list_pending_submit_effects_for_grid blocking task")?
    }
}

#[async_trait]
impl GridReadRepositoryPort for SqliteStorage {
    async fn list_grid_snapshots(&self) -> Result<Vec<StoredGridSnapshot>> {
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || Self::list_grid_snapshots_blocking(conn))
            .await
            .context("failed to join list_grid_snapshots blocking task")?
    }

    async fn load_grid_snapshot(&self, grid_id: &GridId) -> Result<Option<StoredGridSnapshot>> {
        let conn = Arc::clone(&self.conn);
        let grid_id = grid_id.as_str().to_owned();

        tokio::task::spawn_blocking(move || Self::load_grid_snapshot_blocking(conn, grid_id))
            .await
            .context("failed to join load_grid_snapshot blocking task")?
    }

    async fn list_recent_grid_events(
        &self,
        grid_id: &GridId,
        limit: usize,
    ) -> Result<Vec<StoredDomainEvent>> {
        let conn = Arc::clone(&self.conn);
        let grid_id = grid_id.clone();

        tokio::task::spawn_blocking(move || {
            Self::list_recent_grid_events_blocking(conn, grid_id, limit)
        })
        .await
        .context("failed to join list_recent_grid_events blocking task")?
    }

    async fn list_recent_grid_effects(
        &self,
        grid_id: &GridId,
        limit: usize,
    ) -> Result<Vec<PersistedGridEffect>> {
        let conn = Arc::clone(&self.conn);
        let grid_id = grid_id.clone();

        tokio::task::spawn_blocking(move || {
            Self::list_recent_grid_effects_blocking(conn, grid_id, limit)
        })
        .await
        .context("failed to join list_recent_grid_effects blocking task")?
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
    use grid_engine::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot};
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::ports::{
        EffectStatus, GridReadRepositoryPort, OrderRequest, OrderStatus, StateRepositoryPort,
    };
    use grid_engine::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, GridStatus, RiskState, SlotState,
        WorkingOrder,
    };
    use grid_engine::snapshot::{GridRuntimeSnapshot, ObservedState};
    use grid_engine::transition::GridEffect;
    use rusqlite::Connection;

    fn test_snapshot() -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: GridId::new("test-1"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
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
            replacement_gate_reason: None,
            risk: RiskState {
                realized_pnl_day: Some(NaiveDate::from_ymd_opt(2026, 3, 24).unwrap()),
                realized_pnl_today: 12.5,
                realized_pnl_cumulative: 17.5,
                unrealized_pnl: -3.0,
            },
            executor_state: ExecutorState {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(2.0),
                gap_started_at: Some(
                    DateTime::parse_from_rfc3339("2026-03-24T07:31:00+00:00")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                last_reprice_at: Some(
                    DateTime::parse_from_rfc3339("2026-03-24T07:32:00+00:00")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                slots: vec![ExecutionSlot {
                    slot: OrderSlot::new("passive_buy_1"),
                    state: SlotState::Working,
                    working_order: Some(WorkingOrder {
                        order_id: Some("order-1".into()),
                        client_order_id: "client-1".into(),
                        side: Side::Buy,
                        price: 94.5,
                        quantity: 0.25,
                        target_exposure: Exposure(6.0),
                        status: OrderStatus::New,
                        role: OrderRole::IncreaseInventory,
                    }),
                }],
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
                stats: ExecutionStats {
                    started_at: DateTime::parse_from_rfc3339("2026-03-24T07:30:00+00:00")
                        .unwrap()
                        .with_timezone(&Utc),
                    max_inventory_gap_abs: Exposure(3.5),
                    max_gap_age_ms: 42_000,
                },
            },
            observed: ObservedState {
                reference_price: Some(95.0),
                out_of_band_since: Some(
                    DateTime::parse_from_rfc3339("2026-03-24T07:30:00+00:00")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
            },
        }
    }

    fn test_event() -> DomainEvent {
        DomainEvent::BandBreached {
            boundary: BandBoundary::Above,
            price: 120.0,
        }
    }

    fn test_order_request() -> OrderRequest {
        test_order_request_for_symbol("BTCUSDT")
    }

    fn test_order_request_for_symbol(symbol: &str) -> OrderRequest {
        OrderRequest {
            instrument: Instrument::new(Venue::Binance, symbol),
            side: Side::Buy,
            price: 95.0,
            quantity: 0.25,
            client_order_id: "client-2".into(),
            reduce_only: false,
        }
    }

    fn test_snapshot_for(grid_id: &str, symbol: &str) -> GridRuntimeSnapshot {
        let mut snapshot = test_snapshot();
        snapshot.grid_id = GridId::new(grid_id);
        snapshot.instrument = Instrument::new(Venue::Binance, symbol);
        snapshot
    }

    async fn persist_two_events_for(grid_id: &str, storage: &SqliteStorage) -> [DomainEvent; 2] {
        let snapshot = test_snapshot_for(grid_id, "BTCUSDT");
        let first_event = DomainEvent::BandBreached {
            boundary: BandBoundary::Above,
            price: 120.0,
        };
        let second_event = DomainEvent::BandReentered { price: 100.0 };

        storage
            .save_transition(grid_id, &snapshot, &[first_event], &[])
            .await
            .unwrap();
        storage
            .save_transition(grid_id, &snapshot, &[second_event], &[])
            .await
            .unwrap();

        [
            DomainEvent::BandBreached {
                boundary: BandBoundary::Above,
                price: 120.0,
            },
            DomainEvent::BandReentered { price: 100.0 },
        ]
    }

    async fn save_effect_status_update(
        storage: &SqliteStorage,
        grid_id: &str,
        effect_status_update: EffectStatusUpdate,
    ) {
        let snapshot = storage
            .load_grid_state(grid_id)
            .await
            .unwrap()
            .expect("snapshot should exist before updating effect status");
        storage
            .save_transition_with_effect_status(
                grid_id,
                &snapshot,
                &[],
                &[],
                Some(&effect_status_update),
            )
            .await
            .unwrap();
    }

    async fn persist_effect_batches_for_two_grids(
        storage: &SqliteStorage,
    ) -> [PersistedGridEffect; 2] {
        let btc_snapshot = test_snapshot_for("btc-core", "BTCUSDT");
        let eth_snapshot = test_snapshot_for("eth-core", "ETHUSDT");
        let submit_effect = GridEffect::SubmitOrder {
            request: test_order_request(),
            target_exposure: Exposure(6.0),
        };

        let first_btc = storage
            .save_transition("btc-core", &btc_snapshot, &[], &[submit_effect.clone()])
            .await
            .unwrap()
            .effects
            .into_iter()
            .next()
            .unwrap();
        let second_btc = storage
            .save_transition(
                "btc-core",
                &btc_snapshot,
                &[],
                &[GridEffect::CancelAll {
                    instrument: btc_snapshot.instrument.clone(),
                }],
            )
            .await
            .unwrap()
            .effects
            .into_iter()
            .next()
            .unwrap();
        storage
            .save_transition("eth-core", &eth_snapshot, &[], &[submit_effect])
            .await
            .unwrap();

        [first_btc, second_btc]
    }

    async fn persist_three_effect_batches_for_grid(
        storage: &SqliteStorage,
        grid_id: &str,
        symbol: &str,
    ) -> [PersistedGridEffect; 3] {
        let snapshot = test_snapshot_for(grid_id, symbol);

        let first = storage
            .save_transition(
                grid_id,
                &snapshot,
                &[],
                &[GridEffect::CancelAll {
                    instrument: snapshot.instrument.clone(),
                }],
            )
            .await
            .unwrap()
            .effects
            .into_iter()
            .next()
            .unwrap();
        let second = storage
            .save_transition(
                grid_id,
                &snapshot,
                &[],
                &[GridEffect::SubmitOrder {
                    request: test_order_request(),
                    target_exposure: Exposure(6.0),
                }],
            )
            .await
            .unwrap()
            .effects
            .into_iter()
            .next()
            .unwrap();
        let third = storage
            .save_transition(
                grid_id,
                &snapshot,
                &[],
                &[GridEffect::CancelAll {
                    instrument: snapshot.instrument.clone(),
                }],
            )
            .await
            .unwrap()
            .effects
            .into_iter()
            .next()
            .unwrap();

        [first, second, third]
    }

    fn overwrite_effect_updated_at(storage: &SqliteStorage, effect_id: &str, updated_at: &str) {
        let conn = storage.conn.lock().unwrap();
        conn.execute(
            "UPDATE grid_effects
             SET updated_at = ?1
             WHERE effect_id = ?2",
            params![updated_at, effect_id],
        )
        .unwrap();
    }

    fn overwrite_snapshot_updated_at(storage: &SqliteStorage, grid_id: &str, updated_at: &str) {
        let conn = storage.conn.lock().unwrap();
        conn.execute(
            "UPDATE grid_snapshots
             SET updated_at = ?1
             WHERE grid_id = ?2",
            params![updated_at, grid_id],
        )
        .unwrap();
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
            .save_transition("test-1", &snapshot, &[], &[])
            .await
            .unwrap();
        let loaded = storage.load_grid_state("test-1").await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.grid_id.as_str(), "test-1");
        assert_eq!(loaded.instrument.symbol, "BTCUSDT");
        assert_eq!(loaded.status, GridStatus::Active);
        assert_eq!(loaded.config, snapshot.config);
        assert!((loaded.current_exposure.0 - 4.0).abs() < f64::EPSILON);
        assert_eq!(loaded.target_exposure, Some(Exposure(6.0)));
        assert_eq!(
            loaded.risk.realized_pnl_day,
            Some(NaiveDate::from_ymd_opt(2026, 3, 24).unwrap())
        );
        assert!((loaded.risk.realized_pnl_today - 12.5).abs() < f64::EPSILON);
        assert!((loaded.risk.unrealized_pnl + 3.0).abs() < f64::EPSILON);
        assert_eq!(loaded.observed.reference_price, Some(95.0));
        assert_eq!(
            loaded.observed.out_of_band_since,
            snapshot.observed.out_of_band_since
        );
    }

    #[tokio::test]
    async fn save_and_load_grid_runtime_snapshot_roundtrip() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition(snapshot.grid_id.as_str(), &snapshot, &[], &[])
            .await
            .unwrap();

        let loaded = storage
            .load_grid_state(snapshot.grid_id.as_str())
            .await
            .unwrap()
            .unwrap();
        assert!((loaded.risk.realized_pnl_cumulative - 17.5).abs() < f64::EPSILON);
        assert_eq!(loaded, snapshot);
    }

    #[tokio::test]
    async fn saves_and_loads_executor_state_with_working_orders() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition(snapshot.grid_id.as_str(), &snapshot, &[], &[])
            .await
            .unwrap();

        let loaded = storage
            .load_grid_state(snapshot.grid_id.as_str())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(loaded.executor_state, snapshot.executor_state);
        assert_eq!(
            loaded.executor_state.stats.started_at,
            snapshot.executor_state.stats.started_at
        );
    }

    #[tokio::test]
    async fn save_transition_persists_snapshot_and_events_atomically() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition("test-1", &snapshot, &[test_event()], &[])
            .await
            .unwrap();

        let loaded = storage.load_grid_state("test-1").await.unwrap().unwrap();
        let events = storage.list_events("test-1").await.unwrap();

        assert_eq!(loaded.grid_id.as_str(), "test-1");
        assert_eq!(events, vec![test_event()]);
    }

    #[tokio::test]
    async fn save_transition_persists_snapshot_events_and_effects_atomically() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();
        let effects = vec![GridEffect::SubmitOrder {
            request: test_order_request(),
            target_exposure: Exposure(6.0),
        }];

        let persisted = storage
            .save_transition("test-1", &snapshot, &[test_event()], &effects)
            .await
            .unwrap();

        let loaded = storage.load_grid_state("test-1").await.unwrap().unwrap();
        let events = storage.list_events("test-1").await.unwrap();
        let pending = storage.list_dispatchable_effects().await.unwrap();

        assert_eq!(loaded.grid_id.as_str(), "test-1");
        assert_eq!(events, vec![test_event()]);
        assert_eq!(persisted.effects, pending);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].grid_id.as_str(), "test-1");
        assert_eq!(pending[0].effect, effects[0]);
        assert_eq!(pending[0].status, EffectStatus::Pending);
        assert_eq!(pending[0].attempt_count, 0);
        assert_eq!(pending[0].last_error, None);
    }

    #[tokio::test]
    async fn save_transition_with_effect_status_records_failed_attempt_count_and_last_error() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();
        let effects = vec![GridEffect::SubmitOrder {
            request: test_order_request(),
            target_exposure: Exposure(6.0),
        }];

        let persisted = storage
            .save_transition("test-1", &snapshot, &[], &effects)
            .await
            .unwrap();
        let effect_id = persisted.effects[0].effect_id.clone();

        save_effect_status_update(
            &storage,
            "test-1",
            EffectStatusUpdate {
                effect_id: effect_id.clone(),
                status: EffectStatus::Failed,
                attempt_delta: 1,
                last_error: Some("submit order rejected".into()),
            },
        )
        .await;

        let conn = storage.conn.lock().unwrap();
        let effect_row = conn
            .query_row(
                "SELECT status, attempt_count, last_error
                 FROM grid_effects
                 WHERE effect_id = ?1",
                params![effect_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(effect_row.0, "failed");
        assert_eq!(effect_row.1, 1);
        assert_eq!(effect_row.2.as_deref(), Some("submit order rejected"));
    }

    #[tokio::test]
    async fn list_pending_effects_only_returns_batch_head_until_prior_effect_succeeds() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();
        let effects = vec![
            GridEffect::CancelAll {
                instrument: snapshot.instrument.clone(),
            },
            GridEffect::SubmitOrder {
                request: test_order_request(),
                target_exposure: Exposure(6.0),
            },
        ];

        let persisted = storage
            .save_transition("test-1", &snapshot, &[], &effects)
            .await
            .unwrap();

        let pending = storage.list_dispatchable_effects().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].effect_id, persisted.effects[0].effect_id);

        save_effect_status_update(
            &storage,
            "test-1",
            EffectStatusUpdate::succeeded(persisted.effects[0].effect_id.clone()),
        )
        .await;

        let pending = storage.list_dispatchable_effects().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].effect_id, persisted.effects[1].effect_id);
    }

    #[tokio::test]
    async fn list_pending_effects_advances_after_prior_effect_is_superseded() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();
        let effects = vec![
            GridEffect::CancelAll {
                instrument: snapshot.instrument.clone(),
            },
            GridEffect::SubmitOrder {
                request: test_order_request(),
                target_exposure: Exposure(6.0),
            },
        ];

        let persisted = storage
            .save_transition("test-1", &snapshot, &[], &effects)
            .await
            .unwrap();

        save_effect_status_update(
            &storage,
            "test-1",
            EffectStatusUpdate::superseded(persisted.effects[0].effect_id.clone()),
        )
        .await;

        let pending = storage.list_dispatchable_effects().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].effect_id, persisted.effects[1].effect_id);
    }

    #[tokio::test]
    async fn list_pending_effects_keeps_follow_up_blocked_after_prior_failure() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();
        let effects = vec![
            GridEffect::CancelAll {
                instrument: snapshot.instrument.clone(),
            },
            GridEffect::SubmitOrder {
                request: test_order_request(),
                target_exposure: Exposure(6.0),
            },
        ];

        let persisted = storage
            .save_transition("test-1", &snapshot, &[], &effects)
            .await
            .unwrap();

        save_effect_status_update(
            &storage,
            "test-1",
            EffectStatusUpdate {
                effect_id: persisted.effects[0].effect_id.clone(),
                status: EffectStatus::Failed,
                attempt_delta: 1,
                last_error: Some("cancel rejected".into()),
            },
        )
        .await;

        let pending = storage.list_dispatchable_effects().await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn list_pending_submit_effects_for_grid_returns_only_dispatchable_submit_effects() {
        let storage = SqliteStorage::in_memory().unwrap();
        let btc_snapshot = test_snapshot_for("btc-core", "BTCUSDT");
        let eth_snapshot = test_snapshot_for("eth-core", "ETHUSDT");

        let btc_effects = vec![
            GridEffect::CancelAll {
                instrument: btc_snapshot.instrument.clone(),
            },
            GridEffect::SubmitOrder {
                request: test_order_request_for_symbol("BTCUSDT"),
                target_exposure: Exposure(6.0),
            },
        ];
        let eth_effects = vec![GridEffect::SubmitOrder {
            request: test_order_request_for_symbol("ETHUSDT"),
            target_exposure: Exposure(3.0),
        }];

        let btc_persisted = storage
            .save_transition("btc-core", &btc_snapshot, &[], &btc_effects)
            .await
            .unwrap();
        storage
            .save_transition("eth-core", &eth_snapshot, &[], &eth_effects)
            .await
            .unwrap();

        assert!(
            storage
                .list_pending_submit_effects_for_grid(&GridId::new("btc-core"))
                .await
                .unwrap()
                .is_empty()
        );

        save_effect_status_update(
            &storage,
            "btc-core",
            EffectStatusUpdate::succeeded(btc_persisted.effects[0].effect_id.clone()),
        )
        .await;

        let btc_submit_hints = storage
            .list_pending_submit_effects_for_grid(&GridId::new("btc-core"))
            .await
            .unwrap();
        assert_eq!(btc_submit_hints.len(), 1);
        assert_eq!(btc_submit_hints[0].grid_id.as_str(), "btc-core");
        assert!(matches!(
            btc_submit_hints[0].effect,
            GridEffect::SubmitOrder { .. }
        ));
    }

    #[tokio::test]
    async fn list_recent_grid_events_returns_timestamped_records_in_order() {
        let storage = SqliteStorage::in_memory().unwrap();
        let expected_events = persist_two_events_for("btc-core", &storage).await;

        let events = storage
            .list_recent_grid_events(&GridId::new("btc-core"), 10)
            .await
            .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].grid_id.as_str(), "btc-core");
        assert_eq!(events[1].grid_id.as_str(), "btc-core");
        assert_eq!(events[0].event, expected_events[0]);
        assert_eq!(events[1].event, expected_events[1]);
        assert!(events[0].id < events[1].id);
        assert!(events[0].created_at <= events[1].created_at);
    }

    #[tokio::test]
    async fn list_recent_grid_effects_filters_by_grid_id_and_limit() {
        let storage = SqliteStorage::in_memory().unwrap();
        let [oldest_btc_effect, newest_btc_effect] =
            persist_effect_batches_for_two_grids(&storage).await;

        let effects = storage
            .list_recent_grid_effects(&GridId::new("btc-core"), 1)
            .await
            .unwrap();

        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].grid_id.as_str(), "btc-core");
        assert_eq!(effects[0].effect_id, newest_btc_effect.effect_id);
        assert_eq!(effects[0].batch_id, newest_btc_effect.batch_id);
        assert_eq!(effects[0].sequence, newest_btc_effect.sequence);
        assert_eq!(effects[0].effect, newest_btc_effect.effect);
        assert_ne!(effects[0].effect_id, oldest_btc_effect.effect_id);
    }

    #[tokio::test]
    async fn list_recent_grid_effects_orders_results_by_updated_at() {
        let storage = SqliteStorage::in_memory().unwrap();
        let [first, second, third] =
            persist_three_effect_batches_for_grid(&storage, "btc-core", "BTCUSDT").await;

        overwrite_effect_updated_at(&storage, &first.effect_id, "2026-03-24T10:00:03+00:00");
        overwrite_effect_updated_at(&storage, &second.effect_id, "2026-03-24T10:00:01+00:00");
        overwrite_effect_updated_at(&storage, &third.effect_id, "2026-03-24T10:00:02+00:00");

        let effects = storage
            .list_recent_grid_effects(&GridId::new("btc-core"), 3)
            .await
            .unwrap();

        assert_eq!(effects.len(), 3);
        assert_eq!(
            effects
                .iter()
                .map(|effect| effect.effect_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                second.effect_id.as_str(),
                third.effect_id.as_str(),
                first.effect_id.as_str(),
            ]
        );
        assert!(effects[0].updated_at < effects[1].updated_at);
        assert!(effects[1].updated_at < effects[2].updated_at);
    }

    #[tokio::test]
    async fn list_recent_grid_effects_includes_status_updated_effect_in_recent_window() {
        let storage = SqliteStorage::in_memory().unwrap();
        let [first, second, third] =
            persist_three_effect_batches_for_grid(&storage, "btc-core", "BTCUSDT").await;

        overwrite_effect_updated_at(&storage, &first.effect_id, "2026-03-24T10:00:00+00:00");
        overwrite_effect_updated_at(&storage, &second.effect_id, "2026-03-24T10:00:01+00:00");
        overwrite_effect_updated_at(&storage, &third.effect_id, "2026-03-24T10:00:02+00:00");

        save_effect_status_update(
            &storage,
            "btc-core",
            EffectStatusUpdate {
                effect_id: first.effect_id.clone(),
                status: EffectStatus::Failed,
                attempt_delta: 1,
                last_error: Some("submit order rejected".into()),
            },
        )
        .await;

        let effects = storage
            .list_recent_grid_effects(&GridId::new("btc-core"), 2)
            .await
            .unwrap();

        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].effect_id, third.effect_id);
        assert_eq!(effects[1].effect_id, first.effect_id);
        assert_eq!(effects[1].status, EffectStatus::Failed);
        assert_eq!(effects[1].attempt_count, 1);
        assert_eq!(
            effects[1].last_error.as_deref(),
            Some("submit order rejected")
        );
    }

    #[tokio::test]
    async fn list_grid_snapshots_returns_persisted_updated_at() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot_for("btc-core", "BTCUSDT");

        storage
            .save_transition("btc-core", &snapshot, &[], &[])
            .await
            .unwrap();
        overwrite_snapshot_updated_at(&storage, "btc-core", "2026-03-26T10:01:30+00:00");

        let snapshots = storage.list_grid_snapshots().await.unwrap();

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].snapshot.grid_id.as_str(), "btc-core");
        assert_eq!(
            snapshots[0].updated_at,
            DateTime::parse_from_rfc3339("2026-03-26T10:01:30+00:00")
                .unwrap()
                .with_timezone(&Utc)
        );
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
            .save_transition("test-1", &snapshot, &[], &[])
            .await
            .unwrap();

        snapshot.current_exposure = Exposure(6.0);
        snapshot.observed.reference_price = Some(96.0);
        storage
            .save_transition("test-1", &snapshot, &[], &[])
            .await
            .unwrap();

        let loaded = storage.load_grid_state("test-1").await.unwrap().unwrap();
        assert!((loaded.current_exposure.0 - 6.0).abs() < f64::EPSILON);
        assert_eq!(loaded.observed.reference_price, Some(96.0));
    }

    #[tokio::test]
    async fn save_rejects_mismatched_snapshot_id() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        let result = storage
            .save_transition("different-id", &snapshot, &[], &[])
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
                .save_transition("test-1", &snapshot, &[], &[])
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
            .save_transition("test-1", &snapshot, &[], &[])
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
        conn.execute_batch(
            "CREATE TABLE grid_snapshots (
                grid_id TEXT PRIMARY KEY,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
                status TEXT NOT NULL,
                current_exposure REAL NOT NULL,
                reference_price REAL,
                target_exposure REAL,
                pending_order_json TEXT,
                replacement_gate_reason_json TEXT,
                realized_pnl_day TEXT,
                realized_pnl_today REAL NOT NULL DEFAULT 0,
                realized_pnl_cumulative REAL NOT NULL DEFAULT 0,
                unrealized_pnl REAL NOT NULL DEFAULT 0,
                out_of_band_since TEXT,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO grid_snapshots (
                grid_id,
                venue,
                symbol,
                config_json,
                status,
                current_exposure,
                reference_price,
                target_exposure,
                pending_order_json,
                replacement_gate_reason_json,
                realized_pnl_day,
                realized_pnl_today,
                realized_pnl_cumulative,
                unrealized_pnl,
                out_of_band_since,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL, 0, 0, 0, 0, NULL, ?8)",
            params![
                "legacy-grid",
                "binance",
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
