use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use poise_application::{
    self as app, CommittedTrackWrite, EffectStatus, EffectStatusUpdate, FollowUpRetirementRequest,
    PersistedTrackEffect, StoredTrackEvent, StoredTrackSnapshot, TrackEffectStore,
    TrackMutationStore, TrackQueryStore,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema;
use poise_core::events::DomainEvent;
use poise_engine::persisted_runtime::{PersistedRuntimeCodec, PersistedRuntimeRow};
use poise_engine::snapshot::TrackRuntimeSnapshot;
use poise_engine::track::TrackId;
use poise_engine::transition::TrackEffect;

pub struct SqliteStorage {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountMonitorObservedSnapshotRow {
    pub equity: f64,
    pub available: f64,
    pub unrealized_pnl: f64,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountMonitorStateRow {
    pub trading_day: NaiveDate,
    pub baseline_equity: f64,
    pub baseline_captured_at: DateTime<Utc>,
    pub last_observed_snapshot: Option<AccountMonitorObservedSnapshotRow>,
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
        Self::backfill_restore_revision_from_legacy_definition_columns(&conn)
            .context("failed to migrate legacy track definitions into restore_revision")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .with_context(|| format!("failed to inspect sqlite table `{table}`"))?;
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .with_context(|| format!("failed to query sqlite table info for `{table}`"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("failed to deserialize sqlite table info for `{table}`"))?;
        Ok(columns.iter().any(|candidate| candidate == column))
    }

    fn backfill_restore_revision_from_legacy_definition_columns(conn: &Connection) -> Result<()> {
        let has_venue = Self::table_has_column(conn, "track_snapshots", "venue")?;
        let has_symbol = Self::table_has_column(conn, "track_snapshots", "symbol")?;
        let has_config_json = Self::table_has_column(conn, "track_snapshots", "config_json")?;

        let legacy_column_count = [has_venue, has_symbol, has_config_json]
            .into_iter()
            .filter(|present| *present)
            .count();
        if legacy_column_count == 0 {
            return Ok(());
        }
        if legacy_column_count != 3 {
            return Err(anyhow!(
                "track_snapshots contains partial legacy definition columns; expected venue, symbol, and config_json together"
            ));
        }

        conn.execute_batch("BEGIN IMMEDIATE")
            .context("failed to begin restore_revision backfill transaction")?;
        let result = (|| -> Result<()> {
            let pending_rows = {
                let mut stmt = conn
                    .prepare(
                        "SELECT track_id, venue, symbol, config_json
                         FROM track_snapshots
                         WHERE restore_revision IS NULL",
                    )
                    .context(
                        "failed to query legacy track definitions for restore revision backfill",
                    )?;
                stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .context(
                    "failed to iterate legacy track definitions for restore revision backfill",
                )?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context(
                    "failed to deserialize legacy track definitions for restore revision backfill",
                )?
            };

            for (track_id, venue, symbol, config_json) in pending_rows {
                let (venue, symbol, config_json) = match (venue, symbol, config_json) {
                    (Some(venue), Some(symbol), Some(config_json)) => (venue, symbol, config_json),
                    _ => {
                        return Err(anyhow!(
                            "track `{track_id}` is missing legacy definition columns required to backfill restore_revision"
                        ));
                    }
                };
                let restore_revision =
                    PersistedRuntimeCodec::restore_revision_from_legacy_definition(
                        &venue,
                        &symbol,
                        &config_json,
                    )?;
                conn.execute(
                    "UPDATE track_snapshots
                     SET restore_revision = ?1
                     WHERE track_id = ?2",
                    params![restore_revision.as_str(), track_id],
                )
                .context("failed to backfill restore_revision from legacy track definition")?;
            }

            Ok(())
        })();
        match result {
            Ok(()) => conn
                .execute_batch("COMMIT")
                .context("failed to commit restore_revision backfill transaction"),
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn lock_connection(conn: &Mutex<Connection>) -> Result<MutexGuard<'_, Connection>> {
        conn.lock()
            .map_err(|err| anyhow!("failed to lock sqlite connection: {err}"))
    }

    fn deserialize_domain_event(event_json: &str) -> rusqlite::Result<DomainEvent> {
        serde_json::from_str(event_json).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
        })
    }

    fn deserialize_track_effect(effect_json: &str) -> rusqlite::Result<TrackEffect> {
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

    fn deserialize_trading_day(value: &str, column: usize) -> rusqlite::Result<NaiveDate> {
        NaiveDate::parse_from_str(value, "%F").map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(err),
            )
        })
    }

    fn deserialize_account_monitor_snapshot(
        equity: Option<f64>,
        available: Option<f64>,
        unrealized_pnl: Option<f64>,
        observed_at: Option<String>,
    ) -> rusqlite::Result<Option<AccountMonitorObservedSnapshotRow>> {
        match (equity, available, unrealized_pnl, observed_at) {
            (None, None, None, None) => Ok(None),
            (Some(equity), Some(available), Some(unrealized_pnl), Some(observed_at)) => {
                Ok(Some(AccountMonitorObservedSnapshotRow {
                    equity,
                    available,
                    unrealized_pnl,
                    observed_at: Self::deserialize_timestamp(&observed_at, 6)?,
                }))
            }
            _ => Err(rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "account monitor snapshot columns must be all present or all absent",
                )),
            )),
        }
    }

    fn persisted_effect_from_row(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<PersistedTrackEffect> {
        let effect_id = row.get::<_, String>(0)?;
        let track_id = TrackId::new(row.get::<_, String>(1)?);
        let batch_id = row.get::<_, String>(2)?;
        let sequence = row.get::<_, i64>(3)?;
        let effect_json = row.get::<_, String>(4)?;
        let status_text = row.get::<_, String>(5)?;
        let attempt_count = row.get::<_, i64>(6)?;
        let created_at = row.get::<_, String>(8)?;
        let updated_at = row.get::<_, String>(9)?;

        Ok(PersistedTrackEffect {
            effect_id,
            track_id,
            batch_id,
            sequence: u32::try_from(sequence).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Integer,
                    Box::new(err),
                )
            })?,
            effect: Self::deserialize_track_effect(&effect_json)?,
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

    fn load_account_monitor_state_row_blocking(
        conn: Arc<Mutex<Connection>>,
    ) -> Result<Option<AccountMonitorStateRow>> {
        let conn = Self::lock_connection(&conn)?;
        let row = conn
            .query_row(
                "SELECT trading_day,
                        baseline_equity,
                        baseline_captured_at,
                        last_observed_equity,
                        last_observed_available,
                        last_observed_unrealized_pnl,
                        last_observed_at
                 FROM account_monitor_state
                 WHERE singleton_key = 1",
                [],
                |row| {
                    let trading_day: String = row.get(0)?;
                    let baseline_captured_at: String = row.get(2)?;
                    let last_observed_equity: Option<f64> = row.get(3)?;
                    let last_observed_available: Option<f64> = row.get(4)?;
                    let last_observed_unrealized_pnl: Option<f64> = row.get(5)?;
                    let last_observed_at: Option<String> = row.get(6)?;

                    Ok(AccountMonitorStateRow {
                        trading_day: Self::deserialize_trading_day(&trading_day, 0)?,
                        baseline_equity: row.get(1)?,
                        baseline_captured_at: Self::deserialize_timestamp(
                            &baseline_captured_at,
                            2,
                        )?,
                        last_observed_snapshot: Self::deserialize_account_monitor_snapshot(
                            last_observed_equity,
                            last_observed_available,
                            last_observed_unrealized_pnl,
                            last_observed_at,
                        )?,
                    })
                },
            )
            .optional()
            .context("failed to load account monitor state")?;

        Ok(row)
    }

    fn save_account_monitor_state_row_blocking(
        conn: Arc<Mutex<Connection>>,
        row: AccountMonitorStateRow,
    ) -> Result<()> {
        let conn = Self::lock_connection(&conn)?;
        conn.execute(
            "INSERT INTO account_monitor_state (
                singleton_key,
                trading_day,
                baseline_equity,
                baseline_captured_at,
                last_observed_equity,
                last_observed_available,
                last_observed_unrealized_pnl,
                last_observed_at
            ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(singleton_key) DO UPDATE SET
                trading_day = excluded.trading_day,
                baseline_equity = excluded.baseline_equity,
                baseline_captured_at = excluded.baseline_captured_at,
                last_observed_equity = excluded.last_observed_equity,
                last_observed_available = excluded.last_observed_available,
                last_observed_unrealized_pnl = excluded.last_observed_unrealized_pnl,
                last_observed_at = excluded.last_observed_at",
            params![
                row.trading_day.format("%F").to_string(),
                row.baseline_equity,
                row.baseline_captured_at.to_rfc3339(),
                row.last_observed_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.equity),
                row.last_observed_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.available),
                row.last_observed_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.unrealized_pnl),
                row.last_observed_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.observed_at.to_rfc3339()),
            ],
        )
        .context("failed to save account monitor state")?;
        Ok(())
    }

    pub async fn load_account_monitor_state_row(&self) -> Result<Option<AccountMonitorStateRow>> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || Self::load_account_monitor_state_row_blocking(conn))
            .await
            .context("failed to join load_account_monitor_state_row blocking task")?
    }

    fn list_persisted_track_presence_blocking(
        conn: Arc<Mutex<Connection>>,
    ) -> Result<Vec<TrackId>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT track_id
                 FROM persisted_track_presence
                 ORDER BY track_id ASC",
            )
            .context("failed to prepare persisted track presence query")?;
        let track_ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .context("failed to query persisted track presence")?
            .map(|track_id| track_id.map(TrackId::new))
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize persisted track presence")?;
        Ok(track_ids)
    }

    pub async fn list_persisted_track_presence(&self) -> Result<Vec<TrackId>> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || Self::list_persisted_track_presence_blocking(conn))
            .await
            .context("failed to join list_persisted_track_presence blocking task")?
    }

    pub async fn save_account_monitor_state_row(&self, row: &AccountMonitorStateRow) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let row = row.clone();
        tokio::task::spawn_blocking(move || {
            Self::save_account_monitor_state_row_blocking(conn, row)
        })
        .await
        .context("failed to join save_account_monitor_state_row blocking task")?
    }

    fn save_transition_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
        state: TrackRuntimeSnapshot,
        events: Vec<DomainEvent>,
        effects: Vec<TrackEffect>,
        effect_status_update: Option<EffectStatusUpdate>,
    ) -> Result<CommittedTrackWrite> {
        let status_json =
            serde_json::to_string(&state.status).context("failed to serialize track status")?;
        let executor_state_json = serde_json::to_string(&state.executor_state)
            .context("failed to serialize executor state")?;
        let replacement_gate_reason_json = state
            .replacement_gate_reason
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("failed to serialize replacement gate reason")?;
        let ledger_state = state.ledger_state.clone();
        let ledger_state_json =
            serde_json::to_string(&ledger_state).context("failed to serialize ledger state")?;
        let out_of_band_since = state
            .observed
            .out_of_band_since
            .map(|value| value.to_rfc3339());
        let last_tick_at = state.observed.last_tick_at.map(|value| value.to_rfc3339());
        let market_data_stale_since = state
            .observed
            .market_data_stale_since
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
            "INSERT OR REPLACE INTO track_snapshots (
                track_id,
                restore_revision,
                status,
                current_exposure,
                desired_exposure,
                manual_target_override,
                executor_state_json,
                replacement_gate_reason_json,
                ledger_state_json,
                unrealized_pnl,
                reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                id,
                state.restore_revision.as_str(),
                status_json,
                state.current_exposure.0,
                state.desired_exposure.as_ref().map(|exposure| exposure.0),
                state
                    .manual_target_override
                    .as_ref()
                    .map(|exposure| exposure.0),
                executor_state_json,
                replacement_gate_reason_json,
                ledger_state_json,
                state.risk.unrealized_pnl,
                state.observed.reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
                updated_at_text
            ],
        )
        .context("failed to save track snapshot")?;

        tx.execute(
            "INSERT INTO persisted_track_presence (track_id, created_at, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(track_id) DO UPDATE SET
                 updated_at = excluded.updated_at",
            params![id, updated_at_text, updated_at_text],
        )
        .context("failed to upsert persisted track presence")?;

        for event in events {
            let event_json =
                serde_json::to_string(&event).context("failed to serialize domain event")?;
            tx.execute(
                "INSERT INTO track_events (track_id, event_json, created_at)
                 VALUES (?1, ?2, ?3)",
                params![id, event_json, updated_at_text],
            )
            .context("failed to save domain event")?;
        }

        let track_id = TrackId::new(id.clone());
        let mut persisted_effects = Vec::new();
        for (index, effect) in effects.into_iter().enumerate() {
            if matches!(effect, TrackEffect::NoOp) {
                continue;
            }

            let effect_id = format!("{id}:{batch_nonce}:{index}");
            let sequence = u32::try_from(index).context("effect sequence overflow")?;
            let effect_json =
                serde_json::to_string(&effect).context("failed to serialize track effect")?;
            tx.execute(
                "INSERT INTO track_effects (
                    effect_id,
                    track_id,
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
            .context("failed to save track effect")?;

            persisted_effects.push(PersistedTrackEffect {
                effect_id,
                track_id: track_id.clone(),
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
                    "UPDATE track_effects
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
                .context("failed to update track effect status in transition transaction")?;
            ensure!(
                changed == 1,
                "effect status update affected {changed} rows in transition transaction"
            );
        }

        tx.commit()
            .context("failed to commit sqlite transition transaction")?;

        Ok(CommittedTrackWrite {
            track_id,
            effects: persisted_effects,
        })
    }

    fn load_track_state_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
    ) -> Result<Option<TrackRuntimeSnapshot>> {
        let conn = Self::lock_connection(&conn)?;
        let snapshot = conn
            .query_row(
                "SELECT track_id, status, current_exposure, desired_exposure,
                        restore_revision,
                        manual_target_override,
                        executor_state_json, replacement_gate_reason_json, ledger_state_json,
                        realized_pnl_day, realized_pnl_today, realized_pnl_cumulative, unrealized_pnl,
                        reference_price, out_of_band_since, last_tick_at, market_data_stale_since
                 FROM track_snapshots
                 WHERE track_id = ?1",
                params![id],
                Self::track_snapshot_from_row,
            )
            .optional()
            .context("failed to load track snapshot")?;

        Ok(snapshot)
    }

    fn load_track_snapshot_blocking(
        conn: Arc<Mutex<Connection>>,
        id: String,
    ) -> Result<Option<StoredTrackSnapshot>> {
        let conn = Self::lock_connection(&conn)?;
        let snapshot = conn
            .query_row(
                "SELECT track_id, status, current_exposure, desired_exposure,
                        restore_revision,
                        manual_target_override,
                        executor_state_json, replacement_gate_reason_json, ledger_state_json,
                        realized_pnl_day, realized_pnl_today, realized_pnl_cumulative, unrealized_pnl,
                        reference_price, out_of_band_since, last_tick_at, market_data_stale_since, updated_at
                 FROM track_snapshots
                 WHERE track_id = ?1",
                params![id],
                Self::stored_track_snapshot_from_row,
            )
            .optional()
            .context("failed to load track snapshot record")?;

        Ok(snapshot)
    }

    fn track_snapshot_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackRuntimeSnapshot> {
        let runtime = PersistedRuntimeCodec::decode_row(PersistedRuntimeRow {
            track_id: TrackId::new(row.get::<_, String>(0)?),
            status_json: row.get(1)?,
            current_exposure: row.get(2)?,
            desired_exposure: row.get(3)?,
            restore_revision: row.get(4)?,
            manual_target_override: row.get(5)?,
            executor_state_json: row.get(6)?,
            replacement_gate_reason_json: row.get(7)?,
            ledger_state_json: row.get(8)?,
            realized_pnl_day: row.get(9)?,
            realized_pnl_today: row.get(10)?,
            realized_pnl_cumulative: row.get(11)?,
            unrealized_pnl: row.get(12)?,
            reference_price: row.get(13)?,
            out_of_band_since: row.get(14)?,
            last_tick_at: row.get(15)?,
            market_data_stale_since: row.get(16)?,
        })
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    err.to_string(),
                )),
            )
        })?;

        Ok(runtime)
    }

    fn stored_track_snapshot_from_row(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<StoredTrackSnapshot> {
        let updated_at: String = row.get(17)?;

        Ok(StoredTrackSnapshot {
            snapshot: Self::track_snapshot_from_row(row)?,
            updated_at: Self::deserialize_timestamp(&updated_at, 17)?,
        })
    }

    fn list_track_snapshots_blocking(
        conn: Arc<Mutex<Connection>>,
    ) -> Result<Vec<StoredTrackSnapshot>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT track_id, status, current_exposure, desired_exposure,
                        restore_revision,
                        manual_target_override,
                        executor_state_json, replacement_gate_reason_json, ledger_state_json,
                        realized_pnl_day, realized_pnl_today, realized_pnl_cumulative, unrealized_pnl,
                        reference_price, out_of_band_since, last_tick_at, market_data_stale_since, updated_at
                 FROM track_snapshots
                 ORDER BY track_id ASC",
            )
            .context("failed to prepare track snapshot list query")?;

        let snapshots = stmt
            .query_map([], Self::stored_track_snapshot_from_row)
            .context("failed to query track snapshots")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize track snapshots")?;
        Ok(snapshots)
    }

    fn list_events_blocking(conn: Arc<Mutex<Connection>>, id: String) -> Result<Vec<DomainEvent>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT event_json
                 FROM track_events
                 WHERE track_id = ?1
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

    fn list_recent_track_events_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
        limit: usize,
    ) -> Result<Vec<StoredTrackEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).context("event limit overflow")?;
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, track_id, event_json, created_at
                 FROM track_events
                 WHERE track_id = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?2",
            )
            .context("failed to prepare recent domain event query")?;

        let mut events = stmt
            .query_map(params![track_id.as_str(), limit], |row| {
                let event_json: String = row.get(2)?;
                let created_at: String = row.get(3)?;
                Ok(StoredTrackEvent {
                    id: row.get(0)?,
                    track_id: TrackId::new(row.get::<_, String>(1)?),
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

    fn list_recent_track_effects_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
        limit: usize,
    ) -> Result<Vec<PersistedTrackEffect>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).context("effect limit overflow")?;
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT effect_id, track_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM track_effects
                 WHERE track_id = ?1
                 ORDER BY updated_at DESC, created_at DESC, batch_id DESC, sequence DESC, effect_id DESC
                 LIMIT ?2",
            )
            .context("failed to prepare recent track effect query")?;

        let mut effects = stmt
            .query_map(
                params![track_id.as_str(), limit],
                Self::persisted_effect_from_row,
            )
            .context("failed to query recent track effects")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize recent track effects")?;
        effects.reverse();
        Ok(effects)
    }

    fn list_dispatchable_effects_blocking(
        conn: Arc<Mutex<Connection>>,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT effect_id, track_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM track_effects ge
                 WHERE ge.status = ?1
                   AND NOT EXISTS (
                       SELECT 1
                       FROM track_effects prior
                       WHERE prior.track_id = ge.track_id
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

    fn list_pending_submit_effects_for_track_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT effect_id, track_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM track_effects ge
                 WHERE ge.track_id = ?1
                   AND ge.status = ?2
                   AND NOT EXISTS (
                       SELECT 1
                       FROM track_effects prior
                       WHERE prior.track_id = ge.track_id
                         AND prior.batch_id = ge.batch_id
                         AND prior.sequence < ge.sequence
                         AND prior.status NOT IN (?3, ?4)
                   )
                 ORDER BY ge.created_at ASC, ge.batch_id ASC, ge.sequence ASC, ge.effect_id ASC",
            )
            .context("failed to prepare track-scoped pending submit effect query")?;

        let effects = stmt
            .query_map(
                params![
                    track_id.as_str(),
                    EffectStatus::Pending.as_str(),
                    EffectStatus::Succeeded.as_str(),
                    EffectStatus::Superseded.as_str()
                ],
                Self::persisted_effect_from_row,
            )
            .context("failed to query track-scoped pending submit effects")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize track-scoped pending submit effects")?;

        Ok(effects
            .into_iter()
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .collect())
    }

    fn list_all_pending_submit_effects_blocking(
        conn: Arc<Mutex<Connection>>,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT effect_id, track_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM track_effects
                 WHERE status = ?1
                 ORDER BY created_at ASC, batch_id ASC, sequence ASC, effect_id ASC",
            )
            .context("failed to prepare all pending submit effect query")?;

        let effects = stmt
            .query_map(
                params![EffectStatus::Pending.as_str()],
                Self::persisted_effect_from_row,
            )
            .context("failed to query all pending submit effects")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize all pending submit effects")?;

        Ok(effects
            .into_iter()
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .collect())
    }

    fn list_pending_submit_effects_for_track_batch_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
        batch_id: String,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT effect_id, track_id, batch_id, sequence, effect_json, status, attempt_count, last_error, created_at, updated_at
                 FROM track_effects
                 WHERE track_id = ?1
                   AND batch_id = ?2
                   AND status = ?3
                 ORDER BY sequence ASC, effect_id ASC",
            )
            .context("failed to prepare batch-scoped pending submit effect query")?;

        let effects = stmt
            .query_map(
                params![track_id.as_str(), batch_id, EffectStatus::Pending.as_str()],
                Self::persisted_effect_from_row,
            )
            .context("failed to query batch-scoped pending submit effects")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to deserialize batch-scoped pending submit effects")?;

        Ok(effects
            .into_iter()
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .collect())
    }

    fn save_follow_up_retirement_request_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
        request: FollowUpRetirementRequest,
    ) -> Result<()> {
        let conn = Self::lock_connection(&conn)?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO follow_up_retirements (
                 track_id, batch_id, blocked_sequence, closed_order_id, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(track_id, batch_id, blocked_sequence, closed_order_id)
             DO UPDATE SET updated_at = excluded.updated_at",
            params![
                track_id.as_str(),
                request.batch_id,
                request.blocked_sequence,
                request.closed_order_id,
                now,
                now
            ],
        )
        .context("failed to upsert follow-up retirement request")?;
        Ok(())
    }

    fn list_follow_up_retirement_requests_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
    ) -> Result<Vec<FollowUpRetirementRequest>> {
        let conn = Self::lock_connection(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT batch_id, blocked_sequence, closed_order_id
                 FROM follow_up_retirements
                 WHERE track_id = ?1
                 ORDER BY updated_at ASC, batch_id ASC, blocked_sequence ASC, closed_order_id ASC",
            )
            .context("failed to prepare follow-up retirement request query")?;

        stmt.query_map(params![track_id.as_str()], |row| {
            Ok(FollowUpRetirementRequest {
                batch_id: row.get(0)?,
                blocked_sequence: row.get(1)?,
                closed_order_id: row.get(2)?,
            })
        })
        .context("failed to query follow-up retirement requests")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to deserialize follow-up retirement requests")
    }

    fn delete_follow_up_retirement_request_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
        request: FollowUpRetirementRequest,
    ) -> Result<()> {
        let conn = Self::lock_connection(&conn)?;
        conn.execute(
            "DELETE FROM follow_up_retirements
             WHERE track_id = ?1
               AND batch_id = ?2
               AND blocked_sequence = ?3
               AND closed_order_id = ?4",
            params![
                track_id.as_str(),
                request.batch_id,
                request.blocked_sequence,
                request.closed_order_id
            ],
        )
        .context("failed to delete follow-up retirement request")?;
        Ok(())
    }
}

#[async_trait]
impl TrackMutationStore for SqliteStorage {
    async fn save_transition_with_effect_status(
        &self,
        id: &str,
        state: &TrackRuntimeSnapshot,
        events: &[DomainEvent],
        effects: &[TrackEffect],
        effect_status_update: Option<&EffectStatusUpdate>,
    ) -> Result<CommittedTrackWrite> {
        ensure!(
            id == state.track_id.as_str(),
            "snapshot id mismatch: key `{id}` does not match snapshot.track_id `{}`",
            state.track_id.as_str()
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

    async fn load_track_state(&self, id: &str) -> Result<Option<TrackRuntimeSnapshot>> {
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();

        tokio::task::spawn_blocking(move || Self::load_track_state_blocking(conn, id))
            .await
            .context("failed to join load_track_state blocking task")?
    }

    async fn list_track_events(&self, id: &str) -> Result<Vec<DomainEvent>> {
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();

        tokio::task::spawn_blocking(move || Self::list_events_blocking(conn, id))
            .await
            .context("failed to join list_events blocking task")?
    }
}

#[async_trait]
impl TrackEffectStore for SqliteStorage {
    async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || Self::list_dispatchable_effects_blocking(conn))
            .await
            .context("failed to join list_dispatchable_effects blocking task")?
    }

    async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || Self::list_all_pending_submit_effects_blocking(conn))
            .await
            .context("failed to join list_all_pending_submit_effects blocking task")?
    }

    async fn list_pending_submit_effects_for_track(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();

        tokio::task::spawn_blocking(move || {
            Self::list_pending_submit_effects_for_track_blocking(conn, track_id)
        })
        .await
        .context("failed to join list_pending_submit_effects_for_track blocking task")?
    }

    async fn list_pending_submit_effects_for_track_batch(
        &self,
        track_id: &TrackId,
        batch_id: &str,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();
        let batch_id = batch_id.to_string();

        tokio::task::spawn_blocking(move || {
            Self::list_pending_submit_effects_for_track_batch_blocking(conn, track_id, batch_id)
        })
        .await
        .context("failed to join list_pending_submit_effects_for_track_batch blocking task")?
    }

    async fn save_follow_up_retirement_request(
        &self,
        track_id: &TrackId,
        request: &FollowUpRetirementRequest,
    ) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();
        let request = request.clone();

        tokio::task::spawn_blocking(move || {
            Self::save_follow_up_retirement_request_blocking(conn, track_id, request)
        })
        .await
        .context("failed to join save_follow_up_retirement_request blocking task")?
    }

    async fn list_follow_up_retirement_requests(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<FollowUpRetirementRequest>> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();

        tokio::task::spawn_blocking(move || {
            Self::list_follow_up_retirement_requests_blocking(conn, track_id)
        })
        .await
        .context("failed to join list_follow_up_retirement_requests blocking task")?
    }

    async fn delete_follow_up_retirement_request(
        &self,
        track_id: &TrackId,
        request: &FollowUpRetirementRequest,
    ) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();
        let request = request.clone();

        tokio::task::spawn_blocking(move || {
            Self::delete_follow_up_retirement_request_blocking(conn, track_id, request)
        })
        .await
        .context("failed to join delete_follow_up_retirement_request blocking task")?
    }
}

#[async_trait]
impl TrackQueryStore for SqliteStorage {
    async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || Self::list_track_snapshots_blocking(conn))
            .await
            .context("failed to join list_track_snapshots blocking task")?
    }

    async fn load_track_snapshot(&self, track_id: &TrackId) -> Result<Option<StoredTrackSnapshot>> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.as_str().to_owned();

        tokio::task::spawn_blocking(move || Self::load_track_snapshot_blocking(conn, track_id))
            .await
            .context("failed to join load_track_snapshot blocking task")?
    }

    async fn list_recent_track_events(
        &self,
        track_id: &TrackId,
        limit: usize,
    ) -> Result<Vec<StoredTrackEvent>> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();

        tokio::task::spawn_blocking(move || {
            Self::list_recent_track_events_blocking(conn, track_id, limit)
        })
        .await
        .context("failed to join list_recent_track_events blocking task")?
    }

    async fn list_recent_track_effects(
        &self,
        track_id: &TrackId,
        limit: usize,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();

        tokio::task::spawn_blocking(move || {
            Self::list_recent_track_effects_blocking(conn, track_id, limit)
        })
        .await
        .context("failed to join list_recent_track_effects blocking task")?
    }
}

#[async_trait]
impl app::AccountMonitorStore for SqliteStorage {
    async fn load_state(&self) -> Result<Option<app::StoredAccountMonitorState>> {
        Ok(self
            .load_account_monitor_state_row()
            .await?
            .map(|row| app::StoredAccountMonitorState {
                trading_day: row.trading_day,
                baseline_equity: row.baseline_equity,
                baseline_captured_at: row.baseline_captured_at,
                last_observed_account_snapshot: row.last_observed_snapshot.map(|snapshot| {
                    poise_engine::ports::AccountSummarySnapshot {
                        equity: snapshot.equity,
                        available: snapshot.available,
                        unrealized_pnl: snapshot.unrealized_pnl,
                        observed_at: snapshot.observed_at,
                    }
                }),
            }))
    }

    async fn save_state(&self, state: &app::StoredAccountMonitorState) -> Result<()> {
        let row = AccountMonitorStateRow {
            trading_day: state.trading_day,
            baseline_equity: state.baseline_equity,
            baseline_captured_at: state.baseline_captured_at,
            last_observed_snapshot: state
                .last_observed_account_snapshot
                .as_ref()
                .map(|snapshot| AccountMonitorObservedSnapshotRow {
                    equity: snapshot.equity,
                    available: snapshot.available,
                    unrealized_pnl: snapshot.unrealized_pnl,
                    observed_at: snapshot.observed_at,
                }),
        };
        self.save_account_monitor_state_row(&row).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use std::env;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use std::time::{SystemTime, UNIX_EPOCH};

    use poise_application::{
        EffectStatus, FollowUpRetirementRequest, TrackEffectStore, TrackMutationStore,
        TrackQueryStore,
    };
    use poise_core::events::DomainEvent;
    use poise_core::strategy::BandBoundary;
    use poise_core::strategy::{
        DEFAULT_MIN_REBALANCE_UNITS, OutOfBandPolicy, ShapeFamily, TrackConfig,
    };
    use poise_core::types::{Exposure, Side};
    use poise_engine::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot};
    use poise_engine::ledger::{LedgerGapReason, LedgerGapRecord, TrackLedgerState};
    use poise_engine::persisted_runtime::TrackRestoreRevision;
    use poise_engine::ports::{OrderRequest, OrderStatus};
    use poise_engine::runtime::{
        ExecutionRound, ExecutionSlot, ExecutionStats, ExecutorDiagnostics, ExecutorState,
        RiskState, SlotState, TrackStatus, WorkingOrder,
    };
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use rusqlite::Connection;

    fn test_track_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: DEFAULT_MIN_REBALANCE_UNITS,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn test_instrument(symbol: &str) -> Instrument {
        Instrument::new(Venue::Binance, symbol)
    }

    fn test_snapshot() -> TrackRuntimeSnapshot {
        let instrument = test_instrument("BTCUSDT");
        let config = test_track_config();

        TrackRuntimeSnapshot {
            track_id: TrackId::new("test-1"),
            restore_revision: TrackRestoreRevision::for_track(&instrument, &config),
            status: TrackStatus::Active,
            current_exposure: Exposure(4.0),
            desired_exposure: Some(Exposure(6.0)),
            manual_target_override: Some(Exposure(0.0)),
            replacement_gate_reason: None,
            ledger_state: TrackLedgerState {
                realized_pnl_day: Some(NaiveDate::from_ymd_opt(2026, 3, 24).unwrap()),
                gross_realized_pnl_today: 12.5,
                gross_realized_pnl_cumulative: 17.5,
                trading_fee_today: 1.2,
                trading_fee_cumulative: 3.2,
                funding_fee_today: -0.5,
                funding_fee_cumulative: -1.5,
                unresolved_gaps: vec![
                    LedgerGapRecord {
                        gap_key: "binance:order_trade_update:btcusdt:12345:commission_asset".into(),
                        reason: LedgerGapReason::UnsupportedCommissionAsset,
                        observed_at: DateTime::parse_from_rfc3339("2026-03-24T07:35:00+00:00")
                            .unwrap()
                            .with_timezone(&Utc),
                        source: "binance:order_trade_update".into(),
                    },
                    LedgerGapRecord {
                        gap_key:
                            "binance:funding_fee:btcusdt:2026-03-24T08:00:00+00:00:missing_symbol"
                                .into(),
                        reason: LedgerGapReason::MissingSymbol,
                        observed_at: DateTime::parse_from_rfc3339("2026-03-24T08:00:00+00:00")
                            .unwrap()
                            .with_timezone(&Utc),
                        source: "binance:account_update".into(),
                    },
                ],
            },
            risk: RiskState {
                unrealized_pnl: -3.0,
                ..RiskState::default()
            },
            executor_state: ExecutorState {
                active_round: Some(ExecutionRound {
                    desired_exposure: Exposure(6.0),
                    mode: ExecutionMode::Passive,
                    started_at: DateTime::parse_from_rfc3339("2026-03-24T07:30:00+00:00")
                        .unwrap()
                        .with_timezone(&Utc),
                }),
                diagnostics: ExecutorDiagnostics {
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
                    last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                    recovery_anomaly: None,
                },
                slots: vec![ExecutionSlot {
                    slot: OrderSlot::new("passive_buy_1"),
                    state: SlotState::Working,
                    working_order: Some(WorkingOrder {
                        order_id: Some("order-1".into()),
                        client_order_id: "client-1".into(),
                        side: Side::Buy,
                        price: 94.5,
                        quantity: 0.25,
                        status: OrderStatus::New,
                        role: OrderRole::IncreaseInventory,
                    }),
                }],
                recent_terminal_orders: Vec::new(),
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
                last_tick_at: None,
                market_data_stale_since: None,
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

    fn test_snapshot_for(track_id: &str, symbol: &str) -> TrackRuntimeSnapshot {
        let mut snapshot = test_snapshot();
        snapshot.track_id = TrackId::new(track_id);
        snapshot.restore_revision =
            TrackRestoreRevision::for_track(&test_instrument(symbol), &test_track_config());
        snapshot
    }

    async fn persist_two_events_for(track_id: &str, storage: &SqliteStorage) -> [DomainEvent; 2] {
        let snapshot = test_snapshot_for(track_id, "BTCUSDT");
        let first_event = DomainEvent::BandBreached {
            boundary: BandBoundary::Above,
            price: 120.0,
        };
        let second_event = DomainEvent::BandReentered { price: 100.0 };

        storage
            .save_transition(track_id, &snapshot, &[first_event], &[])
            .await
            .unwrap();
        storage
            .save_transition(track_id, &snapshot, &[second_event], &[])
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
        track_id: &str,
        effect_status_update: EffectStatusUpdate,
    ) {
        let snapshot = storage
            .load_track_state(track_id)
            .await
            .unwrap()
            .expect("snapshot should exist before updating effect status");
        storage
            .save_transition_with_effect_status(
                track_id,
                &snapshot,
                &[],
                &[],
                Some(&effect_status_update),
            )
            .await
            .unwrap();
    }

    async fn persist_effect_batches_for_two_tracks(
        storage: &SqliteStorage,
    ) -> [PersistedTrackEffect; 2] {
        let btc_snapshot = test_snapshot_for("btc-core", "BTCUSDT");
        let eth_snapshot = test_snapshot_for("eth-core", "ETHUSDT");
        let submit_effect = TrackEffect::SubmitOrder {
            request: test_order_request(),
            desired_exposure: Exposure(6.0),
        };

        let first_btc = storage
            .save_transition(
                "btc-core",
                &btc_snapshot,
                &[],
                std::slice::from_ref(&submit_effect),
            )
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
                &[TrackEffect::CancelAll {
                    instrument: test_instrument("BTCUSDT"),
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
        track_id: &str,
        symbol: &str,
    ) -> [PersistedTrackEffect; 3] {
        let snapshot = test_snapshot_for(track_id, symbol);

        let first = storage
            .save_transition(
                track_id,
                &snapshot,
                &[],
                &[TrackEffect::CancelAll {
                    instrument: test_instrument(symbol),
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
                track_id,
                &snapshot,
                &[],
                &[TrackEffect::SubmitOrder {
                    request: test_order_request(),
                    desired_exposure: Exposure(6.0),
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
                track_id,
                &snapshot,
                &[],
                &[TrackEffect::CancelAll {
                    instrument: test_instrument(symbol),
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
            "UPDATE track_effects
             SET updated_at = ?1
             WHERE effect_id = ?2",
            params![updated_at, effect_id],
        )
        .unwrap();
    }

    fn overwrite_snapshot_updated_at(storage: &SqliteStorage, track_id: &str, updated_at: &str) {
        let conn = storage.conn.lock().unwrap();
        conn.execute(
            "UPDATE track_snapshots
             SET updated_at = ?1
             WHERE track_id = ?2",
            params![updated_at, track_id],
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

        env::temp_dir().join(format!("track-storage-{timestamp}-{counter}.db"))
    }

    #[tokio::test]
    async fn save_and_load_account_monitor_state_round_trip() {
        let storage = SqliteStorage::in_memory().unwrap();
        let expected = AccountMonitorStateRow {
            trading_day: NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
            baseline_equity: 12_500.5,
            baseline_captured_at: Utc.with_ymd_and_hms(2026, 4, 4, 0, 1, 2).unwrap(),
            last_observed_snapshot: Some(AccountMonitorObservedSnapshotRow {
                equity: 12_450.0,
                available: 9_800.0,
                unrealized_pnl: -120.0,
                observed_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 2, 3).unwrap(),
            }),
        };

        storage
            .save_account_monitor_state_row(&expected)
            .await
            .unwrap();

        let actual = storage.load_account_monitor_state_row().await.unwrap();

        assert_eq!(actual, Some(expected));
    }

    #[tokio::test]
    async fn rejects_partial_account_monitor_snapshot_rows() {
        let storage = SqliteStorage::in_memory().unwrap();
        {
            let conn = storage.conn.lock().unwrap();
            conn.execute("DROP TABLE account_monitor_state", [])
                .unwrap();
            conn.execute(
                "CREATE TABLE account_monitor_state (
                    singleton_key INTEGER PRIMARY KEY CHECK (singleton_key = 1),
                    trading_day TEXT NOT NULL,
                    baseline_equity REAL NOT NULL,
                    baseline_captured_at TEXT NOT NULL,
                    last_observed_equity REAL,
                    last_observed_available REAL,
                    last_observed_unrealized_pnl REAL,
                    last_observed_at TEXT
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO account_monitor_state (
                    singleton_key,
                    trading_day,
                    baseline_equity,
                    baseline_captured_at,
                    last_observed_equity,
                    last_observed_available,
                    last_observed_unrealized_pnl,
                    last_observed_at
                ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    "2026-04-04",
                    12_500.5,
                    "2026-04-04T00:01:02+00:00",
                    12_450.0,
                    Option::<f64>::None,
                    -120.0,
                    "2026-04-04T01:02:03+00:00",
                ],
            )
            .unwrap();
        }

        let error = storage
            .load_account_monitor_state_row()
            .await
            .expect_err("partial account snapshot should fail to load");

        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("account monitor snapshot columns must be all present or all absent"),
            "unexpected error: {rendered}"
        );
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition("test-1", &snapshot, &[], &[])
            .await
            .unwrap();
        let loaded = storage.load_track_state("test-1").await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.track_id.as_str(), "test-1");
        assert_eq!(
            loaded.restore_revision,
            TrackRestoreRevision::for_track(&test_instrument("BTCUSDT"), &test_track_config())
        );
        assert_eq!(loaded.status, TrackStatus::Active);
        assert!((loaded.current_exposure.0 - 4.0).abs() < f64::EPSILON);
        assert_eq!(loaded.desired_exposure, Some(Exposure(6.0)));
        assert_eq!(
            loaded.ledger_state.realized_pnl_day,
            Some(NaiveDate::from_ymd_opt(2026, 3, 24).unwrap())
        );
        assert!((loaded.ledger_state.gross_realized_pnl_today - 12.5).abs() < f64::EPSILON);
        assert!((loaded.risk.unrealized_pnl + 3.0).abs() < f64::EPSILON);
        assert_eq!(loaded.observed.reference_price, Some(95.0));
        assert_eq!(
            loaded.observed.out_of_band_since,
            snapshot.observed.out_of_band_since
        );
    }

    #[test]
    fn initialize_creates_persisted_track_presence_table() {
        let storage = SqliteStorage::in_memory().unwrap();
        let conn = storage.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*)
                 FROM sqlite_master
                 WHERE type = 'table' AND name = 'persisted_track_presence'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn save_transition_records_persisted_track_presence() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition("test-1", &snapshot, &[], &[])
            .await
            .unwrap();

        let conn = storage.conn.lock().unwrap();
        let found: Option<String> = conn
            .query_row(
                "SELECT track_id
                 FROM persisted_track_presence
                 WHERE track_id = ?1",
                params!["test-1"],
                |row| row.get(0),
            )
            .optional()
            .unwrap();

        assert_eq!(found.as_deref(), Some("test-1"));
    }

    #[tokio::test]
    async fn save_transition_persists_desired_exposure_column() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition("test-1", &snapshot, &[], &[])
            .await
            .unwrap();

        let conn = storage.conn.lock().unwrap();
        let desired_exposure: Option<f64> = conn
            .query_row(
                "SELECT desired_exposure
                 FROM track_snapshots
                 WHERE track_id = ?1",
                params!["test-1"],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(desired_exposure, Some(6.0));
    }

    #[tokio::test]
    async fn save_transition_leaves_legacy_realized_columns_at_defaults() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition("test-1", &snapshot, &[], &[])
            .await
            .unwrap();

        let conn = storage.conn.lock().unwrap();
        let (realized_pnl_day, realized_pnl_today, realized_pnl_cumulative): (
            Option<String>,
            f64,
            f64,
        ) = conn
            .query_row(
                "SELECT realized_pnl_day, realized_pnl_today, realized_pnl_cumulative
                 FROM track_snapshots
                 WHERE track_id = ?1",
                params!["test-1"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(realized_pnl_day, None);
        assert_eq!(realized_pnl_today, 0.0);
        assert_eq!(realized_pnl_cumulative, 0.0);
    }

    #[tokio::test]
    async fn save_and_load_track_runtime_snapshot_roundtrip() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition(snapshot.track_id.as_str(), &snapshot, &[], &[])
            .await
            .unwrap();

        let loaded = storage
            .load_track_state(snapshot.track_id.as_str())
            .await
            .unwrap()
            .unwrap();
        assert!((loaded.ledger_state.gross_realized_pnl_cumulative - 17.5).abs() < f64::EPSILON);
        assert_eq!(loaded, snapshot);
    }

    #[tokio::test]
    async fn save_and_load_grid_runtime_snapshot_roundtrip() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition(snapshot.track_id.as_str(), &snapshot, &[], &[])
            .await
            .unwrap();

        let ledger_state_json = {
            let conn = storage.conn.lock().unwrap();
            conn.query_row(
                "SELECT ledger_state_json
                 FROM track_snapshots
                 WHERE track_id = ?1",
                params![snapshot.track_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };
        let ledger_state: serde_json::Value = serde_json::from_str(&ledger_state_json).unwrap();
        let gaps = ledger_state["unresolved_gaps"]
            .as_array()
            .expect("ledger_state_json should persist unresolved gaps");
        let loaded = storage
            .load_track_state(snapshot.track_id.as_str())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(gaps.len(), 2);
        assert_eq!(
            gaps[0]["gap_key"],
            json!("binance:order_trade_update:btcusdt:12345:commission_asset")
        );
        assert_eq!(ledger_state["trading_fee_cumulative"], json!(3.2));
        assert_eq!(ledger_state["funding_fee_cumulative"], json!(-1.5));
        assert_eq!(loaded, snapshot);
    }

    #[tokio::test]
    async fn saves_and_loads_executor_state_with_working_orders() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage
            .save_transition(snapshot.track_id.as_str(), &snapshot, &[], &[])
            .await
            .unwrap();

        let loaded = storage
            .load_track_state(snapshot.track_id.as_str())
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

        let loaded = storage.load_track_state("test-1").await.unwrap().unwrap();
        let events = storage.list_track_events("test-1").await.unwrap();

        assert_eq!(loaded.track_id.as_str(), "test-1");
        assert_eq!(events, vec![test_event()]);
    }

    #[tokio::test]
    async fn save_transition_persists_snapshot_events_and_effects_atomically() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();
        let effects = vec![TrackEffect::SubmitOrder {
            request: test_order_request(),
            desired_exposure: Exposure(6.0),
        }];

        let persisted = storage
            .save_transition("test-1", &snapshot, &[test_event()], &effects)
            .await
            .unwrap();

        let loaded = storage.load_track_state("test-1").await.unwrap().unwrap();
        let events = storage.list_track_events("test-1").await.unwrap();
        let pending = storage.list_dispatchable_effects().await.unwrap();

        assert_eq!(loaded.track_id.as_str(), "test-1");
        assert_eq!(events, vec![test_event()]);
        assert_eq!(persisted.effects, pending);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].track_id.as_str(), "test-1");
        assert_eq!(pending[0].effect, effects[0]);
        assert_eq!(pending[0].status, EffectStatus::Pending);
        assert_eq!(pending[0].attempt_count, 0);
        assert_eq!(pending[0].last_error, None);
    }

    #[tokio::test]
    async fn save_transition_with_effect_status_records_failed_attempt_count_and_last_error() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();
        let effects = vec![TrackEffect::SubmitOrder {
            request: test_order_request(),
            desired_exposure: Exposure(6.0),
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
                 FROM track_effects
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
            TrackEffect::CancelAll {
                instrument: test_instrument("BTCUSDT"),
            },
            TrackEffect::SubmitOrder {
                request: test_order_request(),
                desired_exposure: Exposure(6.0),
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
            TrackEffect::CancelAll {
                instrument: test_instrument("BTCUSDT"),
            },
            TrackEffect::SubmitOrder {
                request: test_order_request(),
                desired_exposure: Exposure(6.0),
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
            TrackEffect::CancelAll {
                instrument: test_instrument("BTCUSDT"),
            },
            TrackEffect::SubmitOrder {
                request: test_order_request(),
                desired_exposure: Exposure(6.0),
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
    async fn list_pending_submit_effects_for_track_returns_only_dispatchable_submit_effects() {
        let storage = SqliteStorage::in_memory().unwrap();
        let btc_snapshot = test_snapshot_for("btc-core", "BTCUSDT");
        let eth_snapshot = test_snapshot_for("eth-core", "ETHUSDT");

        let btc_effects = vec![
            TrackEffect::CancelAll {
                instrument: test_instrument("BTCUSDT"),
            },
            TrackEffect::SubmitOrder {
                request: test_order_request_for_symbol("BTCUSDT"),
                desired_exposure: Exposure(6.0),
            },
        ];
        let eth_effects = vec![TrackEffect::SubmitOrder {
            request: test_order_request_for_symbol("ETHUSDT"),
            desired_exposure: Exposure(3.0),
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
                .list_pending_submit_effects_for_track(&TrackId::new("btc-core"))
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
            .list_pending_submit_effects_for_track(&TrackId::new("btc-core"))
            .await
            .unwrap();
        assert_eq!(btc_submit_hints.len(), 1);
        assert_eq!(btc_submit_hints[0].track_id.as_str(), "btc-core");
        assert!(matches!(
            btc_submit_hints[0].effect,
            TrackEffect::SubmitOrder { .. }
        ));
    }

    #[tokio::test]
    async fn list_all_pending_submit_effects_returns_non_dispatchable_pending_submits() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot_for("btc-core", "BTCUSDT");
        let effects = vec![
            TrackEffect::CancelAll {
                instrument: test_instrument("BTCUSDT"),
            },
            TrackEffect::SubmitOrder {
                request: test_order_request_for_symbol("BTCUSDT"),
                desired_exposure: Exposure(6.0),
            },
        ];

        let persisted = storage
            .save_transition("btc-core", &snapshot, &[], &effects)
            .await
            .unwrap();

        let pending = storage.list_all_pending_submit_effects().await.unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].effect_id, persisted.effects[1].effect_id);
        assert!(matches!(pending[0].effect, TrackEffect::SubmitOrder { .. }));
    }

    #[tokio::test]
    async fn list_pending_submit_effects_for_track_batch_returns_same_batch_submit_without_ready_filter()
     {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot_for("btc-core", "BTCUSDT");
        let replacement_effects = vec![
            TrackEffect::CancelOrder {
                instrument: test_instrument("BTCUSDT"),
                order_id: "old-order-1".into(),
            },
            TrackEffect::SubmitOrder {
                request: test_order_request_for_symbol("BTCUSDT"),
                desired_exposure: Exposure(4.0),
            },
        ];
        let unrelated_effects = vec![TrackEffect::SubmitOrder {
            request: OrderRequest {
                client_order_id: "other-batch".into(),
                ..test_order_request_for_symbol("BTCUSDT")
            },
            desired_exposure: Exposure(2.0),
        }];

        let replacement_persisted = storage
            .save_transition("btc-core", &snapshot, &[], &replacement_effects)
            .await
            .unwrap();
        storage
            .save_transition("btc-core", &snapshot, &[], &unrelated_effects)
            .await
            .unwrap();

        let batch_effects = storage
            .list_pending_submit_effects_for_track_batch(
                &TrackId::new("btc-core"),
                &replacement_persisted.effects[0].batch_id,
            )
            .await
            .unwrap();

        assert_eq!(batch_effects.len(), 1);
        assert_eq!(
            batch_effects[0].effect_id,
            replacement_persisted.effects[1].effect_id
        );
        assert_eq!(
            batch_effects[0].batch_id,
            replacement_persisted.effects[0].batch_id
        );
        assert!(matches!(
            batch_effects[0].effect,
            TrackEffect::SubmitOrder { .. }
        ));
    }

    #[tokio::test]
    async fn follow_up_retirement_requests_roundtrip_and_dedupe() {
        let storage = SqliteStorage::in_memory().unwrap();
        let track_id = TrackId::new("btc-core");
        let request = FollowUpRetirementRequest {
            batch_id: "replacement".into(),
            blocked_sequence: 0,
            closed_order_id: "old-order-1".into(),
        };

        storage
            .save_follow_up_retirement_request(&track_id, &request)
            .await
            .unwrap();
        storage
            .save_follow_up_retirement_request(&track_id, &request)
            .await
            .unwrap();

        let stored = storage
            .list_follow_up_retirement_requests(&track_id)
            .await
            .unwrap();
        assert_eq!(stored, vec![request.clone()]);

        storage
            .delete_follow_up_retirement_request(&track_id, &request)
            .await
            .unwrap();

        assert!(
            storage
                .list_follow_up_retirement_requests(&track_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn list_recent_track_events_returns_timestamped_records_in_order() {
        let storage = SqliteStorage::in_memory().unwrap();
        let expected_events = persist_two_events_for("btc-core", &storage).await;

        let events =
            TrackQueryStore::list_recent_track_events(&storage, &TrackId::new("btc-core"), 10)
                .await
                .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].track_id.as_str(), "btc-core");
        assert_eq!(events[1].track_id.as_str(), "btc-core");
        assert_eq!(events[0].event, expected_events[0]);
        assert_eq!(events[1].event, expected_events[1]);
        assert!(events[0].id < events[1].id);
        assert!(events[0].created_at <= events[1].created_at);
    }

    #[tokio::test]
    async fn list_recent_track_effects_filters_by_track_id_and_limit() {
        let storage = SqliteStorage::in_memory().unwrap();
        let [oldest_btc_effect, newest_btc_effect] =
            persist_effect_batches_for_two_tracks(&storage).await;

        let effects =
            TrackQueryStore::list_recent_track_effects(&storage, &TrackId::new("btc-core"), 1)
                .await
                .unwrap();

        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].track_id.as_str(), "btc-core");
        assert_eq!(effects[0].effect_id, newest_btc_effect.effect_id);
        assert_eq!(effects[0].batch_id, newest_btc_effect.batch_id);
        assert_eq!(effects[0].sequence, newest_btc_effect.sequence);
        assert_eq!(effects[0].effect, newest_btc_effect.effect);
        assert_ne!(effects[0].effect_id, oldest_btc_effect.effect_id);
    }

    #[tokio::test]
    async fn list_recent_track_effects_orders_results_by_updated_at() {
        let storage = SqliteStorage::in_memory().unwrap();
        let [first, second, third] =
            persist_three_effect_batches_for_grid(&storage, "btc-core", "BTCUSDT").await;

        overwrite_effect_updated_at(&storage, &first.effect_id, "2026-03-24T10:00:03+00:00");
        overwrite_effect_updated_at(&storage, &second.effect_id, "2026-03-24T10:00:01+00:00");
        overwrite_effect_updated_at(&storage, &third.effect_id, "2026-03-24T10:00:02+00:00");

        let effects =
            TrackQueryStore::list_recent_track_effects(&storage, &TrackId::new("btc-core"), 3)
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
    async fn list_recent_track_effects_includes_status_updated_effect_in_recent_window() {
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

        let effects =
            TrackQueryStore::list_recent_track_effects(&storage, &TrackId::new("btc-core"), 2)
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
    async fn sqlite_storage_lists_recent_track_effects_via_track_query_store() {
        let storage = SqliteStorage::in_memory().unwrap();
        persist_effect_batches_for_two_tracks(&storage).await;

        let effects =
            TrackQueryStore::list_recent_track_effects(&storage, &TrackId::new("btc-core"), 10)
                .await
                .unwrap();

        assert_eq!(effects.len(), 2);
    }

    #[tokio::test]
    async fn list_track_snapshots_returns_persisted_updated_at() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot_for("btc-core", "BTCUSDT");

        storage
            .save_transition("btc-core", &snapshot, &[], &[])
            .await
            .unwrap();
        overwrite_snapshot_updated_at(&storage, "btc-core", "2026-03-26T10:01:30+00:00");

        let snapshots = TrackQueryStore::list_track_snapshots(&storage)
            .await
            .unwrap();

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].snapshot.track_id.as_str(), "btc-core");
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
        let loaded = storage.load_track_state("nonexistent").await.unwrap();
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

        let loaded = storage.load_track_state("test-1").await.unwrap().unwrap();
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
        let loaded = reopened.load_track_state("test-1").await.unwrap();
        assert!(loaded.is_some());

        drop(reopened);
        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn load_track_state_rejects_legacy_snapshot_json() {
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
                desired_exposure REAL,
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
            "INSERT INTO track_snapshots (
                track_id,
                venue,
                symbol,
                config_json,
                status,
                current_exposure,
                reference_price,
                desired_exposure,
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
                "legacy-track",
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

        let result = SqliteStorage::from_connection(conn);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn from_connection_backfills_restore_revision_from_legacy_definition_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                restore_revision TEXT,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
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
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_snapshots (
                track_id,
                restore_revision,
                venue,
                symbol,
                config_json,
                status,
                current_exposure,
                desired_exposure,
                manual_target_override,
                executor_state_json,
                replacement_gate_reason_json,
                ledger_state_json,
                realized_pnl_day,
                realized_pnl_today,
                realized_pnl_cumulative,
                unrealized_pnl,
                reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
                updated_at
            ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, NULL, ?8, NULL, 0, 0, 0, ?9, NULL, NULL, NULL, ?10)",
            params![
                "test-1",
                "binance",
                "BTCUSDT",
                serde_json::json!({
                    "lower_price": 90.0,
                    "upper_price": 110.0,
                    "long_exposure_units": 8.0,
                    "short_exposure_units": 8.0,
                    "notional_per_unit": 375.0,
                    "shape_family": "linear",
                    "out_of_band_policy": "freeze"
                })
                .to_string(),
                "\"active\"",
                0.0,
                serde_json::to_string(&ExecutorState::empty(
                    Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap()
                ))
                .unwrap(),
                serde_json::to_string(&TrackLedgerState::default()).unwrap(),
                95.0,
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();

        let storage = SqliteStorage::from_connection(conn).unwrap();
        let loaded = storage.load_track_state("test-1").await.unwrap().unwrap();
        assert_eq!(
            loaded.restore_revision,
            TrackRestoreRevision::for_track(&test_instrument("BTCUSDT"), &test_track_config())
        );
    }

    #[tokio::test]
    async fn load_track_state_from_runtime_only_snapshot_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                restore_revision TEXT,
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
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_snapshots (
                track_id,
                restore_revision,
                status,
                current_exposure,
                desired_exposure,
                manual_target_override,
                executor_state_json,
                replacement_gate_reason_json,
                ledger_state_json,
                realized_pnl_day,
                realized_pnl_today,
                realized_pnl_cumulative,
                unrealized_pnl,
                reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, NULL, ?6, NULL, 0, 0, 0, ?7, NULL, NULL, NULL, ?8)",
            params![
                "test-1",
                TrackRestoreRevision::for_track(&test_instrument("BTCUSDT"), &test_track_config())
                    .as_str(),
                "\"active\"",
                1.0,
                serde_json::to_string(&ExecutorState::empty(
                    Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap()
                ))
                .unwrap(),
                serde_json::to_string(&TrackLedgerState::default()).unwrap(),
                95.0,
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();

        let storage = SqliteStorage::from_connection(conn).unwrap();
        let loaded = storage.load_track_state("test-1").await.unwrap().unwrap();

        assert_eq!(loaded.track_id.as_str(), "test-1");
        assert_eq!(loaded.restore_revision.as_str().len(), 64);
    }

    #[tokio::test]
    async fn from_connection_rejects_invalid_legacy_config_when_backfilling_restore_revision() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                restore_revision TEXT,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
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
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_snapshots (
                track_id,
                restore_revision,
                venue,
                symbol,
                config_json,
                status,
                current_exposure,
                desired_exposure,
                manual_target_override,
                executor_state_json,
                replacement_gate_reason_json,
                ledger_state_json,
                realized_pnl_day,
                realized_pnl_today,
                realized_pnl_cumulative,
                unrealized_pnl,
                reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
                updated_at
            ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, NULL, ?8, NULL, 0, 0, 0, ?9, NULL, NULL, NULL, ?10)",
            params![
                "test-1",
                "binance",
                "BTCUSDT",
                serde_json::json!({
                    "lower_price": 90.0,
                    "upper_price": 110.0,
                    "long_exposure_units": 8.0,
                    "short_exposure_units": 8.0,
                    "notional_per_unit": 375.0,
                    "min_rebalance_units": -0.1,
                    "shape_family": "linear",
                    "out_of_band_policy": "freeze"
                })
                .to_string(),
                "\"active\"",
                0.0,
                serde_json::to_string(&ExecutorState::empty(
                    Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap()
                ))
                .unwrap(),
                serde_json::to_string(&TrackLedgerState::default()).unwrap(),
                95.0,
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();

        let result = SqliteStorage::from_connection(conn);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn from_connection_rejects_partial_legacy_definition_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                restore_revision TEXT,
                venue TEXT NOT NULL,
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
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_snapshots (
                track_id,
                restore_revision,
                venue,
                status,
                current_exposure,
                desired_exposure,
                manual_target_override,
                executor_state_json,
                replacement_gate_reason_json,
                ledger_state_json,
                realized_pnl_day,
                realized_pnl_today,
                realized_pnl_cumulative,
                unrealized_pnl,
                reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
                updated_at
            ) VALUES (?1, NULL, ?2, ?3, ?4, NULL, NULL, ?5, NULL, ?6, NULL, 0, 0, 0, ?7, NULL, NULL, NULL, ?8)",
            params![
                "test-1",
                "binance",
                "\"active\"",
                0.0,
                serde_json::to_string(&ExecutorState::empty(
                    Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap()
                ))
                .unwrap(),
                serde_json::to_string(&TrackLedgerState::default()).unwrap(),
                95.0,
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();

        let result = SqliteStorage::from_connection(conn);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn backfill_restore_revision_is_atomic_when_a_later_row_is_invalid() {
        let db_path = temp_db_path();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE track_snapshots (
                track_id TEXT PRIMARY KEY,
                restore_revision TEXT,
                venue TEXT NOT NULL,
                symbol TEXT NOT NULL,
                config_json TEXT NOT NULL,
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
            );",
        )
        .unwrap();
        let executor_state_json = serde_json::to_string(&ExecutorState::empty(
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        ))
        .unwrap();
        let ledger_state_json = serde_json::to_string(&TrackLedgerState::default()).unwrap();
        conn.execute(
            "INSERT INTO track_snapshots (
                track_id,
                restore_revision,
                venue,
                symbol,
                config_json,
                status,
                current_exposure,
                desired_exposure,
                manual_target_override,
                executor_state_json,
                replacement_gate_reason_json,
                ledger_state_json,
                realized_pnl_day,
                realized_pnl_today,
                realized_pnl_cumulative,
                unrealized_pnl,
                reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
                updated_at
            ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, NULL, ?8, NULL, 0, 0, 0, ?9, NULL, NULL, NULL, ?10)",
            params![
                "valid-track",
                "binance",
                "BTCUSDT",
                serde_json::json!({
                    "lower_price": 90.0,
                    "upper_price": 110.0,
                    "long_exposure_units": 8.0,
                    "short_exposure_units": 8.0,
                    "notional_per_unit": 375.0,
                    "shape_family": "linear",
                    "out_of_band_policy": "freeze"
                })
                .to_string(),
                "\"active\"",
                0.0,
                executor_state_json,
                ledger_state_json,
                95.0,
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO track_snapshots (
                track_id,
                restore_revision,
                venue,
                symbol,
                config_json,
                status,
                current_exposure,
                desired_exposure,
                manual_target_override,
                executor_state_json,
                replacement_gate_reason_json,
                ledger_state_json,
                realized_pnl_day,
                realized_pnl_today,
                realized_pnl_cumulative,
                unrealized_pnl,
                reference_price,
                out_of_band_since,
                last_tick_at,
                market_data_stale_since,
                updated_at
            ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, NULL, ?8, NULL, 0, 0, 0, ?9, NULL, NULL, NULL, ?10)",
            params![
                "invalid-track",
                "binance",
                "ETHUSDT",
                serde_json::json!({
                    "lower_price": 90.0,
                    "upper_price": 110.0,
                    "long_exposure_units": 8.0,
                    "short_exposure_units": 8.0,
                    "notional_per_unit": 375.0,
                    "min_rebalance_units": -0.1,
                    "shape_family": "linear",
                    "out_of_band_policy": "freeze"
                })
                .to_string(),
                "\"active\"",
                0.0,
                serde_json::to_string(&ExecutorState::empty(
                    Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap()
                ))
                .unwrap(),
                serde_json::to_string(&TrackLedgerState::default()).unwrap(),
                95.0,
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();
        drop(conn);

        let result = SqliteStorage::new(&db_path);
        assert!(result.is_err());

        let verify_conn = Connection::open(&db_path).unwrap();
        let revisions = verify_conn
            .prepare(
                "SELECT restore_revision
                 FROM track_snapshots
                 ORDER BY track_id",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(revisions, vec![None, None]);

        let _ = fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn list_events_rejects_legacy_event_json() {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO track_events (track_id, event_json, created_at)
             VALUES (?1, ?2, ?3)",
            params![
                "BTCUSDT",
                "{\"BandBreached\":{\"boundary\":\"Above\",\"price\":120.0}}",
                "2026-03-25T00:00:00Z"
            ],
        )
        .unwrap();

        let storage = SqliteStorage::from_connection(conn).unwrap();
        let result = storage.list_track_events("BTCUSDT").await;
        assert!(result.is_err());
    }
}
