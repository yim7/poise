use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use poise_application::{
    self as app, CommittedTrackWrite, EffectJournalEntry, EffectStatus, EffectStatusUpdate,
    PersistedTrackEffect, StoredTrackEvent, TrackControlState, TrackEffectJournal,
    TrackMutationStore, TrackQueryStore,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema;
use poise_core::events::DomainEvent;
use poise_core::track::TrackId;
use poise_core::types::Side;
use poise_engine::execution_plan::TrackEffect;
use poise_engine::ledger::{TrackPnlRecord, TrackPnlRecordKind, TrackPnlStats};

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
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
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

    fn load_track_updated_at_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
    ) -> Result<Option<DateTime<Utc>>> {
        let conn = Self::lock_connection(&conn)?;
        let updated_at = conn
            .query_row(
                "SELECT updated_at
                 FROM persisted_track_presence
                 WHERE track_id = ?1",
                params![track_id.as_str()],
                |row| {
                    let value: String = row.get(0)?;
                    Self::deserialize_timestamp(&value, 0)
                },
            )
            .optional()
            .context("failed to load track updated_at")?;
        Ok(updated_at)
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
        control_state: Option<TrackControlState>,
        events: Vec<DomainEvent>,
    ) -> Result<CommittedTrackWrite> {
        let updated_at = Utc::now();
        let updated_at_text = updated_at.to_rfc3339();

        let mut conn = Self::lock_connection(&conn)?;
        let tx = conn
            .transaction()
            .context("failed to start sqlite transition transaction")?;

        let control_state = if control_state.is_none() && !events.is_empty() {
            let has_persisted_control_truth = tx
                .query_row(
                    "SELECT 1 FROM track_control_state WHERE track_id = ?1 LIMIT 1",
                    params![id],
                    |_row| Ok(()),
                )
                .optional()
                .context("failed to check persisted track control state presence")?
                .is_some();
            if has_persisted_control_truth {
                None
            } else {
                Some(TrackControlState::default())
            }
        } else {
            control_state
        };

        if control_state.is_some() || !events.is_empty() {
            tx.execute(
                "INSERT INTO persisted_track_presence (track_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(track_id) DO UPDATE SET
                     updated_at = excluded.updated_at",
                params![id, updated_at_text, updated_at_text],
            )
            .context("failed to upsert persisted track presence")?;
        }

        if let Some(control_state) = control_state {
            let control_state_json = serde_json::to_string(&control_state)
                .context("failed to serialize track control state")?;
            tx.execute(
                "INSERT INTO track_control_state (
                    track_id,
                    control_state_json,
                    updated_at
                ) VALUES (?1, ?2, ?3)
                ON CONFLICT(track_id) DO UPDATE SET
                    control_state_json = excluded.control_state_json,
                    updated_at = excluded.updated_at",
                params![id, control_state_json, updated_at_text],
            )
            .context("failed to save track control state")?;
        }

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

        tx.commit()
            .context("failed to commit sqlite transition transaction")?;

        Ok(CommittedTrackWrite { track_id })
    }

    fn append_effect_journal_entries_blocking(
        conn: Arc<Mutex<Connection>>,
        entries: Vec<EffectJournalEntry>,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut conn = Self::lock_connection(&conn)?;
        let tx = conn
            .transaction()
            .context("failed to start sqlite effect journal append transaction")?;
        for entry in entries {
            let effect = PersistedTrackEffect::from(entry);
            let effect_json = serde_json::to_string(&effect.effect)
                .context("failed to serialize effect journal entry")?;
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
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ON CONFLICT(effect_id) DO NOTHING",
                params![
                    effect.effect_id,
                    effect.track_id.as_str(),
                    effect.batch_id,
                    i64::from(effect.sequence),
                    effect_json,
                    effect.status.as_str(),
                    i64::from(effect.attempt_count),
                    effect.last_error,
                    effect.created_at.to_rfc3339(),
                    effect.updated_at.to_rfc3339(),
                ],
            )
            .context("failed to append effect journal entry")?;
        }
        tx.commit()
            .context("failed to commit sqlite effect journal append transaction")?;
        Ok(())
    }

    fn record_effect_journal_outcomes_blocking(
        conn: Arc<Mutex<Connection>>,
        outcomes: Vec<EffectStatusUpdate>,
    ) -> Result<()> {
        if outcomes.is_empty() {
            return Ok(());
        }

        let updated_at = Utc::now().to_rfc3339();
        let mut conn = Self::lock_connection(&conn)?;
        let tx = conn
            .transaction()
            .context("failed to start sqlite effect journal outcome transaction")?;
        for outcome in outcomes {
            let changed = tx
                .execute(
                    "UPDATE track_effects
                     SET status = ?1,
                         attempt_count = attempt_count + ?2,
                         last_error = ?3,
                         updated_at = ?4
                     WHERE effect_id = ?5",
                    params![
                        outcome.status.as_str(),
                        i64::from(outcome.attempt_delta),
                        outcome.last_error,
                        updated_at,
                        outcome.effect_id
                    ],
                )
                .context("failed to record effect journal outcome")?;
            ensure!(
                changed <= 1,
                "effect journal outcome affected {changed} rows"
            );
        }
        tx.commit()
            .context("failed to commit sqlite effect journal outcome transaction")?;
        Ok(())
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

    fn save_track_control_state_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
        state: TrackControlState,
    ) -> Result<()> {
        let control_state_json =
            serde_json::to_string(&state).context("failed to serialize track control state")?;
        let updated_at = Utc::now().to_rfc3339();
        let mut conn = Self::lock_connection(&conn)?;
        let tx = conn
            .transaction()
            .context("failed to start sqlite track control state transaction")?;

        tx.execute(
            "INSERT INTO track_control_state (
                track_id,
                control_state_json,
                updated_at
            ) VALUES (?1, ?2, ?3)
            ON CONFLICT(track_id) DO UPDATE SET
                control_state_json = excluded.control_state_json,
                updated_at = excluded.updated_at",
            params![track_id.as_str(), control_state_json, updated_at],
        )
        .context("failed to save track control state")?;

        tx.execute(
            "INSERT INTO persisted_track_presence (track_id, created_at, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(track_id) DO UPDATE SET
                 updated_at = excluded.updated_at",
            params![track_id.as_str(), updated_at.as_str(), updated_at.as_str()],
        )
        .context("failed to upsert persisted track presence from track control state")?;
        tx.commit()
            .context("failed to commit sqlite track control state transaction")?;
        Ok(())
    }

    fn load_track_control_state_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
    ) -> Result<Option<TrackControlState>> {
        let conn = Self::lock_connection(&conn)?;
        let row = conn
            .query_row(
                "SELECT control_state_json
                 FROM track_control_state
                 WHERE track_id = ?1",
                params![track_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("failed to load track control state")?;

        row.map(|control_state_json| {
            serde_json::from_str(&control_state_json)
                .context("failed to deserialize track control state")
        })
        .transpose()
    }

    fn insert_track_pnl_record_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
        record: TrackPnlRecord,
    ) -> Result<bool> {
        let mut conn = Self::lock_connection(&conn)?;
        let tx = conn
            .transaction()
            .context("failed to start sqlite track pnl record transaction")?;
        let updated_at = Utc::now().to_rfc3339();
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO track_pnl_records (
                    track_id,
                    venue,
                    symbol,
                    occurred_at,
                    kind,
                    source,
                    source_key,
                    order_id,
                    trade_id,
                    side,
                    price,
                    qty,
                    realized_pnl,
                    trading_fee,
                    funding_fee
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    track_id.as_str(),
                    record.instrument.venue.as_str(),
                    record.instrument.symbol,
                    record.occurred_at.to_rfc3339(),
                    track_pnl_record_kind_as_str(record.kind),
                    record.source,
                    record.source_key,
                    record.order_id,
                    record.trade_id,
                    record.side.map(side_as_str),
                    record.price,
                    record.qty,
                    record.realized_pnl,
                    record.trading_fee,
                    record.funding_fee,
                ],
            )
            .context("failed to insert track pnl record")?
            > 0;

        if inserted {
            tx.execute(
                "INSERT INTO persisted_track_presence (track_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(track_id) DO UPDATE SET
                     updated_at = excluded.updated_at",
                params![track_id.as_str(), updated_at.as_str(), updated_at.as_str()],
            )
            .context("failed to upsert persisted track presence from track pnl record")?;
        }

        tx.commit()
            .context("failed to commit sqlite track pnl record transaction")?;
        Ok(inserted)
    }

    fn load_track_pnl_stats_blocking(
        conn: Arc<Mutex<Connection>>,
        track_id: TrackId,
        pnl_utc_day: NaiveDate,
    ) -> Result<TrackPnlStats> {
        let conn = Self::lock_connection(&conn)?;
        let (gross_realized_pnl_cumulative, trading_fee_cumulative, funding_fee_cumulative) =
            sum_pnl_records(&conn, track_id.as_str(), None, None)?;
        let day_start =
            DateTime::<Utc>::from_naive_utc_and_offset(pnl_utc_day.and_time(NaiveTime::MIN), Utc);
        let next_day = pnl_utc_day
            .succ_opt()
            .ok_or_else(|| anyhow!("invalid pnl utc day `{pnl_utc_day}`"))?;
        let day_end =
            DateTime::<Utc>::from_naive_utc_and_offset(next_day.and_time(NaiveTime::MIN), Utc);
        let (gross_realized_pnl_today, trading_fee_today, funding_fee_today) = sum_pnl_records(
            &conn,
            track_id.as_str(),
            Some(day_start.to_rfc3339()),
            Some(day_end.to_rfc3339()),
        )?;

        Ok(TrackPnlStats {
            pnl_utc_day,
            gross_realized_pnl_today,
            gross_realized_pnl_cumulative,
            trading_fee_today,
            trading_fee_cumulative,
            funding_fee_today,
            funding_fee_cumulative,
            ..TrackPnlStats::default()
        })
    }
}

fn track_pnl_record_kind_as_str(kind: TrackPnlRecordKind) -> &'static str {
    match kind {
        TrackPnlRecordKind::Trade => "trade",
        TrackPnlRecordKind::Funding => "funding",
    }
}

fn side_as_str(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn sum_pnl_records(
    conn: &Connection,
    track_id: &str,
    occurred_at_start: Option<String>,
    occurred_at_end: Option<String>,
) -> Result<(f64, f64, f64)> {
    let mut statement = match (&occurred_at_start, &occurred_at_end) {
        (Some(_), Some(_)) => conn.prepare(
            "SELECT
                COALESCE(SUM(realized_pnl), 0),
                COALESCE(SUM(trading_fee), 0),
                COALESCE(SUM(funding_fee), 0)
             FROM track_pnl_records
             WHERE track_id = ?1
               AND occurred_at >= ?2
               AND occurred_at < ?3",
        )?,
        _ => conn.prepare(
            "SELECT
                COALESCE(SUM(realized_pnl), 0),
                COALESCE(SUM(trading_fee), 0),
                COALESCE(SUM(funding_fee), 0)
             FROM track_pnl_records
             WHERE track_id = ?1",
        )?,
    };

    let sums = match (occurred_at_start, occurred_at_end) {
        (Some(start), Some(end)) => statement.query_row(params![track_id, start, end], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?,
        _ => statement.query_row(params![track_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?,
    };

    Ok(sums)
}

#[async_trait]
impl TrackMutationStore for SqliteStorage {
    async fn commit_track_transition(
        &self,
        id: &str,
        control_state: Option<&TrackControlState>,
        events: &[DomainEvent],
    ) -> Result<CommittedTrackWrite> {
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();
        let control_state = control_state.cloned();
        let events = events.to_vec();

        tokio::task::spawn_blocking(move || {
            Self::save_transition_blocking(conn, id, control_state, events)
        })
        .await
        .context("failed to join commit_track_transition blocking task")?
    }

    async fn list_track_events(&self, id: &str) -> Result<Vec<DomainEvent>> {
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();

        tokio::task::spawn_blocking(move || Self::list_events_blocking(conn, id))
            .await
            .context("failed to join list_events blocking task")?
    }

    async fn save_track_control_state(
        &self,
        track_id: &TrackId,
        state: &TrackControlState,
    ) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();
        let state = state.clone();

        tokio::task::spawn_blocking(move || {
            Self::save_track_control_state_blocking(conn, track_id, state)
        })
        .await
        .context("failed to join save_track_control_state blocking task")?
    }

    async fn insert_track_pnl_record(
        &self,
        track_id: &TrackId,
        record: &TrackPnlRecord,
    ) -> Result<bool> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();
        let record = record.clone();

        tokio::task::spawn_blocking(move || {
            Self::insert_track_pnl_record_blocking(conn, track_id, record)
        })
        .await
        .context("failed to join insert_track_pnl_record blocking task")?
    }
}

#[async_trait]
impl TrackEffectJournal for SqliteStorage {
    async fn append_entries(&self, entries: &[EffectJournalEntry]) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let entries = entries.to_vec();

        tokio::task::spawn_blocking(move || {
            Self::append_effect_journal_entries_blocking(conn, entries)
        })
        .await
        .context("failed to join append_effect_journal_entries blocking task")?
    }

    async fn record_effect_outcomes(&self, outcomes: &[EffectStatusUpdate]) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let outcomes = outcomes.to_vec();

        tokio::task::spawn_blocking(move || {
            Self::record_effect_journal_outcomes_blocking(conn, outcomes)
        })
        .await
        .context("failed to join record_effect_journal_outcomes blocking task")?
    }
}

#[async_trait]
impl TrackQueryStore for SqliteStorage {
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

    async fn load_track_control_state(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackControlState>> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();

        tokio::task::spawn_blocking(move || Self::load_track_control_state_blocking(conn, track_id))
            .await
            .context("failed to join load_track_control_state blocking task")?
    }

    async fn load_track_pnl_stats(
        &self,
        track_id: &TrackId,
        pnl_utc_day: NaiveDate,
    ) -> Result<TrackPnlStats> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();

        tokio::task::spawn_blocking(move || {
            Self::load_track_pnl_stats_blocking(conn, track_id, pnl_utc_day)
        })
        .await
        .context("failed to join load_track_pnl_stats blocking task")?
    }

    async fn load_track_updated_at(&self, track_id: &TrackId) -> Result<Option<DateTime<Utc>>> {
        let conn = Arc::clone(&self.conn);
        let track_id = track_id.clone();

        tokio::task::spawn_blocking(move || Self::load_track_updated_at_blocking(conn, track_id))
            .await
            .context("failed to join load_track_updated_at blocking task")?
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
    use std::env;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use std::time::{SystemTime, UNIX_EPOCH};

    use poise_application::{
        EffectStatus, TrackEffectJournal, TrackMutationStore, TrackQueryStore,
    };
    use poise_core::events::DomainEvent;
    use poise_core::strategy::BandBoundary;
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::ledger::TrackPnlRecord;
    use poise_engine::ports::OrderRequest;
    use rusqlite::Connection;

    fn test_instrument(symbol: &str) -> Instrument {
        Instrument::new(Venue::Binance, symbol)
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

    struct TestCommittedTransition {
        effects: Vec<PersistedTrackEffect>,
    }

    async fn commit_test_transition(
        storage: &SqliteStorage,
        track_id: &str,
        events: &[DomainEvent],
        effects: &[TrackEffect],
    ) -> TestCommittedTransition {
        storage
            .commit_track_transition(track_id, None, events)
            .await
            .unwrap();
        let created_at = Utc::now();
        let batch_id = format!("{}:batch:{}", track_id, created_at.timestamp_micros());
        let entries = effects
            .iter()
            .enumerate()
            .filter_map(|(sequence, effect)| {
                if matches!(effect, TrackEffect::NoOp) {
                    return None;
                }
                Some(EffectJournalEntry {
                    effect_id: format!("{}:{}:{}", track_id, batch_id, sequence),
                    track_id: TrackId::new(track_id),
                    batch_id: batch_id.clone(),
                    sequence: sequence as u32,
                    effect: effect.clone(),
                    created_at,
                })
            })
            .collect::<Vec<_>>();
        storage.append_entries(&entries).await.unwrap();
        TestCommittedTransition {
            effects: entries
                .into_iter()
                .map(PersistedTrackEffect::from)
                .collect(),
        }
    }

    async fn persist_two_events_for(track_id: &str, storage: &SqliteStorage) -> [DomainEvent; 2] {
        let first_event = DomainEvent::BandBreached {
            boundary: BandBoundary::Above,
            price: 120.0,
        };
        let second_event = DomainEvent::BandReentered { price: 100.0 };

        commit_test_transition(storage, track_id, &[first_event], &[]).await;
        commit_test_transition(storage, track_id, &[second_event], &[]).await;

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
        let _ = track_id;
        storage
            .record_effect_outcomes(&[effect_status_update])
            .await
            .unwrap();
    }

    fn insert_effect_without_presence(storage: &SqliteStorage, track_id: &str, effect_id: &str) {
        let effect_json = serde_json::to_string(&TrackEffect::CancelAll {
            instrument: test_instrument("BTCUSDT"),
        })
        .unwrap();
        let now = Utc::now().to_rfc3339();
        storage
            .conn
            .lock()
            .unwrap()
            .execute(
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
                    track_id,
                    format!("{track_id}:batch"),
                    0_i64,
                    effect_json,
                    EffectStatus::Pending.as_str(),
                    0_i64,
                    Option::<String>::None,
                    now,
                    now
                ],
            )
            .unwrap();
    }

    async fn persist_effect_batches_for_two_tracks(
        storage: &SqliteStorage,
    ) -> [PersistedTrackEffect; 2] {
        let submit_effect = TrackEffect::SubmitOrder {
            request: test_order_request(),
            desired_exposure: Exposure(6.0),
            submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            recovery_token: SubmitRecoveryToken::empty(),
        };

        let first_btc = commit_test_transition(
            storage,
            "btc-core",
            &[],
            std::slice::from_ref(&submit_effect),
        )
        .await
        .effects
        .into_iter()
        .next()
        .unwrap();
        let second_btc = commit_test_transition(
            storage,
            "btc-core",
            &[],
            &[TrackEffect::CancelAll {
                instrument: test_instrument("BTCUSDT"),
            }],
        )
        .await
        .effects
        .into_iter()
        .next()
        .unwrap();
        commit_test_transition(storage, "eth-core", &[], &[submit_effect]).await;

        [first_btc, second_btc]
    }

    async fn persist_three_effect_batches_for_grid(
        storage: &SqliteStorage,
        track_id: &str,
        symbol: &str,
    ) -> [PersistedTrackEffect; 3] {
        let first = commit_test_transition(
            storage,
            track_id,
            &[],
            &[TrackEffect::CancelAll {
                instrument: test_instrument(symbol),
            }],
        )
        .await
        .effects
        .into_iter()
        .next()
        .unwrap();
        let second = commit_test_transition(
            storage,
            track_id,
            &[],
            &[TrackEffect::SubmitOrder {
                request: test_order_request(),
                desired_exposure: Exposure(6.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                recovery_token: SubmitRecoveryToken::empty(),
            }],
        )
        .await
        .effects
        .into_iter()
        .next()
        .unwrap();
        let third = commit_test_transition(
            storage,
            track_id,
            &[],
            &[TrackEffect::CancelAll {
                instrument: test_instrument(symbol),
            }],
        )
        .await
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

    fn overwrite_track_presence_updated_at(
        storage: &SqliteStorage,
        track_id: &str,
        updated_at: &str,
    ) {
        let conn = storage.conn.lock().unwrap();
        conn.execute(
            "UPDATE persisted_track_presence
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
    async fn control_state_round_trip_outside_runtime_snapshot() {
        let storage = SqliteStorage::in_memory().unwrap();
        let expected_control = app::TrackControlState::Paused {
            resume_mode: app::PersistedControlMode::ManualFlatten,
        };

        TrackMutationStore::save_track_control_state(
            &storage,
            &TrackId::new("btc-core"),
            &expected_control,
        )
        .await
        .unwrap();
        let actual_control =
            TrackQueryStore::load_track_control_state(&storage, &TrackId::new("btc-core"))
                .await
                .unwrap();

        assert_eq!(actual_control, Some(expected_control));
    }

    #[tokio::test]
    async fn track_pnl_records_aggregate_stats_by_occurred_day() {
        let storage = SqliteStorage::in_memory().unwrap();
        let track_id = TrackId::new("btc-core");

        TrackMutationStore::insert_track_pnl_record(
            &storage,
            &track_id,
            &TrackPnlRecord::trade(
                test_instrument("BTCUSDT"),
                Utc.with_ymd_and_hms(2026, 4, 8, 9, 0, 0).unwrap(),
                "binance:order_trade_update".into(),
                Some("binance:btcusdt:trade:1001".into()),
                Some("order-1".into()),
                Some("1001".into()),
                Side::Sell,
                1900.0,
                0.4,
                120.0,
                3.0,
            ),
        )
        .await
        .unwrap();
        TrackMutationStore::insert_track_pnl_record(
            &storage,
            &track_id,
            &TrackPnlRecord::funding(
                test_instrument("BTCUSDT"),
                Utc.with_ymd_and_hms(2026, 4, 7, 8, 0, 0).unwrap(),
                "binance:funding_fee".into(),
                Some("binance:btcusdt:funding:2026-04-07T08:00:00Z".into()),
                -1.5,
            ),
        )
        .await
        .unwrap();

        let stats = TrackQueryStore::load_track_pnl_stats(
            &storage,
            &track_id,
            NaiveDate::from_ymd_opt(2026, 4, 8).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            stats.pnl_utc_day,
            NaiveDate::from_ymd_opt(2026, 4, 8).unwrap()
        );
        assert_eq!(stats.gross_realized_pnl_today, 120.0);
        assert_eq!(stats.gross_realized_pnl_cumulative, 120.0);
        assert_eq!(stats.trading_fee_today, 3.0);
        assert_eq!(stats.trading_fee_cumulative, 3.0);
        assert_eq!(stats.funding_fee_today, 0.0);
        assert_eq!(stats.funding_fee_cumulative, -1.5);
        assert_eq!(stats.net_realized_pnl_cumulative(), 115.5);
    }

    #[tokio::test]
    async fn saving_control_or_pnl_record_records_persisted_track_presence() {
        let storage = SqliteStorage::in_memory().unwrap();

        TrackMutationStore::save_track_control_state(
            &storage,
            &TrackId::new("btc-core"),
            &app::TrackControlState::default(),
        )
        .await
        .unwrap();
        TrackMutationStore::insert_track_pnl_record(
            &storage,
            &TrackId::new("eth-core"),
            &TrackPnlRecord::trade_summary(
                test_instrument("ETHUSDT"),
                Utc.with_ymd_and_hms(2026, 4, 8, 9, 0, 0).unwrap(),
                "test".into(),
                None,
                None,
                1.0,
                0.0,
            ),
        )
        .await
        .unwrap();

        let found = storage.list_persisted_track_presence().await.unwrap();

        assert_eq!(
            found,
            vec![TrackId::new("btc-core"), TrackId::new("eth-core")]
        );
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
    async fn update_effect_status_without_business_delta_does_not_record_persisted_track_presence()
    {
        let storage = SqliteStorage::in_memory().unwrap();
        insert_effect_without_presence(&storage, "test-1", "effect-1");
        storage
            .record_effect_outcomes(&[EffectStatusUpdate::succeeded("effect-1")])
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

        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn business_transition_records_persisted_track_presence() {
        let storage = SqliteStorage::in_memory().unwrap();
        commit_test_transition(&storage, "test-1", &[test_event()], &[]).await;

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
    async fn load_track_updated_at_reads_persisted_presence_timestamp() {
        let storage = SqliteStorage::in_memory().unwrap();
        commit_test_transition(&storage, "test-1", &[test_event()], &[]).await;
        let updated_at = storage
            .load_track_updated_at(&TrackId::new("test-1"))
            .await
            .unwrap();

        assert!(updated_at.is_some());
    }

    #[tokio::test]
    async fn save_transition_persists_events_atomically() {
        let storage = SqliteStorage::in_memory().unwrap();
        commit_test_transition(&storage, "test-1", &[test_event()], &[]).await;

        let events = storage.list_track_events("test-1").await.unwrap();

        assert_eq!(events, vec![test_event()]);
    }

    #[tokio::test]
    async fn commit_track_transition_backfills_default_control_truth_on_first_business_write() {
        let storage = SqliteStorage::in_memory().unwrap();

        storage
            .commit_track_transition("test-1", None, &[test_event()])
            .await
            .unwrap();

        assert_eq!(
            storage
                .load_track_control_state(&TrackId::new("test-1"))
                .await
                .unwrap(),
            Some(TrackControlState::default())
        );
    }

    #[tokio::test]
    async fn save_transition_persists_events_and_effect_journal_entries_atomically() {
        let storage = SqliteStorage::in_memory().unwrap();
        let effects = vec![TrackEffect::SubmitOrder {
            request: test_order_request(),
            desired_exposure: Exposure(6.0),
            submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            recovery_token: SubmitRecoveryToken::empty(),
        }];

        let persisted = commit_test_transition(&storage, "test-1", &[test_event()], &effects).await;

        let events = storage.list_track_events("test-1").await.unwrap();
        let recent = storage
            .list_recent_track_effects(&TrackId::new("test-1"), 10)
            .await
            .unwrap();

        assert_eq!(events, vec![test_event()]);
        assert_eq!(persisted.effects, recent);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].track_id.as_str(), "test-1");
        assert_eq!(recent[0].effect, effects[0]);
        assert_eq!(recent[0].status, EffectStatus::Pending);
        assert_eq!(recent[0].attempt_count, 0);
        assert_eq!(recent[0].last_error, None);
    }

    #[tokio::test]
    async fn update_effect_status_records_failed_attempt_count_and_last_error() {
        let storage = SqliteStorage::in_memory().unwrap();
        let effects = vec![TrackEffect::SubmitOrder {
            request: test_order_request(),
            desired_exposure: Exposure(6.0),
            submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            recovery_token: SubmitRecoveryToken::empty(),
        }];

        let persisted = commit_test_transition(&storage, "test-1", &[], &effects).await;
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
    async fn update_effect_status_does_not_advance_persisted_track_presence() {
        let storage = SqliteStorage::in_memory().unwrap();
        let effects = vec![TrackEffect::SubmitOrder {
            request: test_order_request(),
            desired_exposure: Exposure(6.0),
            submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            recovery_token: SubmitRecoveryToken::empty(),
        }];
        let persisted = commit_test_transition(&storage, "test-1", &[], &effects).await;
        let effect_id = persisted.effects[0].effect_id.clone();
        let fixed_updated_at = Utc.with_ymd_and_hms(2026, 4, 23, 1, 2, 3).unwrap();
        overwrite_track_presence_updated_at(&storage, "test-1", &fixed_updated_at.to_rfc3339());

        save_effect_status_update(&storage, "test-1", EffectStatusUpdate::succeeded(effect_id))
            .await;

        let updated_at = storage
            .load_track_updated_at(&TrackId::new("test-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated_at, fixed_updated_at);
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
    async fn load_track_updated_at_returns_none_for_nonexistent_track() {
        let storage = SqliteStorage::in_memory().unwrap();
        let updated_at = storage
            .load_track_updated_at(&TrackId::new("nonexistent"))
            .await
            .unwrap();
        assert!(updated_at.is_none());
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
        insert_effect_without_presence(&storage, "test-1", "effect-1");

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
        let save_task = tokio::spawn(async move {
            save_storage
                .record_effect_outcomes(&[EffectStatusUpdate::succeeded("effect-1")])
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
        commit_test_transition(&storage, "test-1", &[test_event()], &[]).await;

        drop(storage);

        let reopened = SqliteStorage::new(&db_path).unwrap();
        let updated_at = reopened
            .load_track_updated_at(&TrackId::new("test-1"))
            .await
            .unwrap();
        assert!(updated_at.is_some());

        drop(reopened);
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
