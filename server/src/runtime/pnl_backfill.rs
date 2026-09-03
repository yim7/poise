use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use poise_application::exchange::{AccountPort, RecentTrackPnlRecords};
use poise_application::{TrackInput, TrackUpdateReceipt};
use poise_core::track::TrackId;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::pnl_audit::required_recent_pnl_window;
use crate::server_context::ReconcileState;

use super::ServerRuntime;

const PNL_BACKFILL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct PnlBackfillSummary {
    pub available_tracks: usize,
    pub unsupported_tracks: usize,
    pub incomplete_tracks: usize,
    pub records_seen: usize,
    pub records_inserted: usize,
    pub records_skipped: usize,
    pub failures: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PnlBackfillSnapshot {
    pub last_completed_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub available_tracks: usize,
    pub unsupported_tracks: usize,
    pub incomplete_tracks: usize,
    pub records_seen: usize,
    pub records_inserted: usize,
    pub records_skipped: usize,
    pub failures: usize,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct PnlBackfillStatus {
    snapshot: RwLock<PnlBackfillSnapshot>,
}

impl PnlBackfillStatus {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn record_completion(&self, completed_at: DateTime<Utc>, summary: &PnlBackfillSummary) {
        let mut snapshot = self.snapshot.write().unwrap();
        snapshot.last_completed_at = Some(completed_at);
        snapshot.available_tracks = summary.available_tracks;
        snapshot.unsupported_tracks = summary.unsupported_tracks;
        snapshot.incomplete_tracks = summary.incomplete_tracks;
        snapshot.records_seen = summary.records_seen;
        snapshot.records_inserted = summary.records_inserted;
        snapshot.records_skipped = summary.records_skipped;
        snapshot.failures = summary.failures;
        let error = if summary.unsupported_tracks > 0 {
            Some(format!(
                "recent PNL recovery is unsupported for {} configured track(s)",
                summary.unsupported_tracks
            ))
        } else if summary.incomplete_tracks > 0 {
            Some(format!(
                "recent PNL coverage is incomplete for {} configured track(s)",
                summary.incomplete_tracks
            ))
        } else if summary.failures > 0 {
            Some(summary.last_error.clone().unwrap_or_else(|| {
                format!(
                    "records_seen={}, records_inserted={}, records_skipped={}, failures={}",
                    summary.records_seen,
                    summary.records_inserted,
                    summary.records_skipped,
                    summary.failures
                )
            }))
        } else {
            None
        };
        match error {
            Some(error) => {
                snapshot.last_error_at = Some(completed_at);
                snapshot.last_error = Some(error);
            }
            None => snapshot.last_success_at = Some(completed_at),
        }
    }

    pub(crate) fn record_success(&self, at: DateTime<Utc>) {
        self.snapshot.write().unwrap().last_success_at = Some(at);
    }

    pub(crate) fn record_success_now(&self) {
        self.record_success(Utc::now());
    }

    pub(crate) fn record_error(&self, at: DateTime<Utc>, error: impl ToString) {
        let mut snapshot = self.snapshot.write().unwrap();
        snapshot.last_error_at = Some(at);
        snapshot.last_error = Some(error.to_string());
    }

    pub(crate) fn record_error_now(&self, error: impl ToString) {
        self.record_error(Utc::now(), error);
    }

    pub(crate) fn snapshot(&self) -> PnlBackfillSnapshot {
        self.snapshot.read().unwrap().clone()
    }

    #[cfg(test)]
    pub(crate) fn replace_snapshot(&self, snapshot: PnlBackfillSnapshot) {
        *self.snapshot.write().unwrap() = snapshot;
    }
}

impl PnlBackfillSnapshot {
    pub(crate) fn health_status(&self) -> super::RuntimeHealthStatus {
        match (self.last_success_at, self.last_error_at) {
            (None, None) => super::RuntimeHealthStatus::Unknown,
            (Some(success), Some(error)) if error > success => super::RuntimeHealthStatus::Degraded,
            (None, Some(_)) => super::RuntimeHealthStatus::Degraded,
            _ => super::RuntimeHealthStatus::Ok,
        }
    }
}

pub(super) fn spawn_pnl_backfill_task(
    runtime: &ServerRuntime,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.reconcile.clone();
    let account = runtime.exchange.account();
    let pnl_backfill_status = Arc::clone(&runtime.state.pnl_backfill_status);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(PNL_BACKFILL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    let summary = backfill_recent_pnl_once(&state, account.as_ref()).await;
                    let completed_at = Utc::now();
                    pnl_backfill_status.record_completion(completed_at, &summary);
                    if summary.records_inserted > 0 {
                        tracing::info!(
                            records_seen = summary.records_seen,
                            records_inserted = summary.records_inserted,
                            records_skipped = summary.records_skipped,
                            failures = summary.failures,
                            "track pnl backfill inserted records"
                        );
                    } else if summary.unsupported_tracks > 0
                        || summary.incomplete_tracks > 0
                        || summary.failures > 0
                    {
                        tracing::warn!(
                            unsupported_tracks = summary.unsupported_tracks,
                            incomplete_tracks = summary.incomplete_tracks,
                            records_seen = summary.records_seen,
                            records_skipped = summary.records_skipped,
                            failures = summary.failures,
                            "track pnl backfill completed with failures"
                        );
                    }
                }
            }
        }
    })
}

pub(super) async fn backfill_recent_pnl_once(
    state: &ReconcileState,
    account: &dyn AccountPort,
) -> PnlBackfillSummary {
    let tracks = state.runtime.track_instruments().await;
    let mut summary = PnlBackfillSummary::default();
    let required_window = required_recent_pnl_window(Utc::now());

    for track in tracks {
        let records = match account
            .get_recent_track_pnl_records(&track.instrument, &required_window)
            .await
        {
            Ok(RecentTrackPnlRecords::Available(batch)) => {
                summary.available_tracks += 1;
                if !batch.covers(&required_window) {
                    summary.incomplete_tracks += 1;
                    summary.last_error = Some(format!(
                        "recent PNL response for track `{}` does not cover {}..{}",
                        track.id, required_window.start, required_window.end
                    ));
                }
                batch.records
            }
            Ok(RecentTrackPnlRecords::Unsupported) => {
                summary.unsupported_tracks += 1;
                tracing::warn!(
                    track_id = track.id,
                    venue = track.instrument.venue.as_str(),
                    symbol = %track.instrument.symbol,
                    "recent track PNL recovery is unsupported"
                );
                continue;
            }
            Err(error) => {
                summary.failures += 1;
                summary.last_error = Some(error.to_string());
                tracing::warn!(
                    track_id = track.id,
                    venue = track.instrument.venue.as_str(),
                    symbol = %track.instrument.symbol,
                    "failed to backfill recent track pnl records: {error}"
                );
                continue;
            }
        };

        for record in records {
            summary.records_seen += 1;
            let track_id = if record.instrument == track.instrument {
                Some(track.id.clone())
            } else {
                state.runtime.resolve_track_id(&record.instrument).await
            };
            let Some(track_id) = track_id else {
                summary.records_skipped += 1;
                tracing::warn!(
                    venue = record.instrument.venue.as_str(),
                    symbol = %record.instrument.symbol,
                    source_key = ?record.source_key,
                    "received backfilled pnl record for unknown track instrument"
                );
                continue;
            };

            match state
                .runtime
                .handle(&TrackId::new(&track_id), TrackInput::TrackPnl(record))
                .await
            {
                Ok(TrackUpdateReceipt::PnlRecorded { inserted: true }) => {
                    summary.records_inserted += 1
                }
                Ok(TrackUpdateReceipt::PnlRecorded { inserted: false }) => {
                    summary.records_skipped += 1
                }
                Ok(_) => unreachable!("track PNL input must return its update receipt"),
                Err(error) => {
                    summary.failures += 1;
                    summary.last_error = Some(error.to_string());
                    tracing::warn!(
                        track_id,
                        "failed to persist backfilled track pnl record: {error}"
                    );
                }
            }
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use poise_application::exchange::{AccountPort, RecentTrackPnlRecords, UserDataEvent};
    use poise_application::{
        TrackEffectJournal, TrackIdentityStore, TrackMutationStore, TrackQueryStore,
    };
    use poise_core::instrument::{Amount, Asset, InstrumentSpec};
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::Side;
    use poise_engine::ledger::TrackPnlRecord;
    use poise_engine::manager::TrackManager;
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use crate::assembly::SystemClock;
    use crate::test_support::{
        build_runtime_and_effect_worker_test_contexts, build_test_application_services,
        unavailable_account_monitor,
    };

    use super::{PnlBackfillStatus, PnlBackfillSummary, backfill_recent_pnl_once};

    fn amount(value: f64, asset: &str) -> Amount {
        Amount::new(value, Asset::try_new(asset).unwrap()).unwrap()
    }

    #[test]
    fn unsupported_backfill_is_recorded_as_degraded_not_success() {
        let status = PnlBackfillStatus::new();
        let completed_at = Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap();
        let summary = PnlBackfillSummary {
            unsupported_tracks: 1,
            ..PnlBackfillSummary::default()
        };

        status.record_completion(completed_at, &summary);

        let status = status.snapshot();
        assert_eq!(status.last_completed_at, Some(completed_at));
        assert_eq!(status.available_tracks, 0);
        assert_eq!(status.unsupported_tracks, 1);
        assert_eq!(
            status.health_status(),
            crate::runtime::RuntimeHealthStatus::Degraded
        );
        assert_eq!(status.last_success_at, None);
        assert!(status.last_error.unwrap().contains("unsupported"));
    }

    #[test]
    fn incomplete_backfill_is_recorded_as_degraded_not_success() {
        let status = PnlBackfillStatus::new();
        let completed_at = Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap();
        let summary = PnlBackfillSummary {
            available_tracks: 1,
            incomplete_tracks: 1,
            ..PnlBackfillSummary::default()
        };

        status.record_completion(completed_at, &summary);

        let snapshot = status.snapshot();
        assert_eq!(snapshot.incomplete_tracks, 1);
        assert_eq!(snapshot.last_success_at, None);
        assert_eq!(
            snapshot.health_status(),
            crate::runtime::RuntimeHealthStatus::Degraded
        );
        assert!(snapshot.last_error.unwrap().contains("incomplete"));
    }

    #[tokio::test]
    async fn backfill_recent_pnl_records_persists_new_records_idempotently() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        TrackIdentityStore::guard_track_identities(
            &*repository,
            &[(
                TrackId::new("btc-coin"),
                Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
            )],
        )
        .await
        .unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager(),
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications);
        let (runtime_context, _) = build_runtime_and_effect_worker_test_contexts(
            &services,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            account_monitor,
        );
        let account = FakeAccount::new(vec![TrackPnlRecord::trade(
            Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
            Utc.with_ymd_and_hms(2026, 6, 11, 2, 16, 1).unwrap(),
            "okx:fills".to_string(),
            Some("okx:orders:btc-usd-swap:460607985".to_string()),
            Some("3645412589617586176".to_string()),
            Some("460607985".to_string()),
            Side::Sell,
            62_096.9,
            poise_engine::ledger::TrackPnlQuantity::base_asset(
                poise_core::instrument::PositionQty::new(0.2).unwrap(),
                poise_core::instrument::Asset::try_new("BTC").unwrap(),
            ),
            amount(0.0, "BTC"),
            Some(amount(0.0000001610386348, "BTC")),
        )]);

        let first =
            backfill_recent_pnl_once(&runtime_context.runtime_state().reconcile, &account).await;
        let second =
            backfill_recent_pnl_once(&runtime_context.runtime_state().reconcile, &account).await;

        assert_eq!(first.records_seen, 1);
        assert_eq!(first.available_tracks, 1);
        assert_eq!(first.unsupported_tracks, 0);
        assert_eq!(first.records_inserted, 1);
        assert_eq!(first.records_skipped, 0);
        assert_eq!(first.failures, 0);
        assert_eq!(first.last_error, None);
        assert_eq!(second.records_seen, 1);
        assert_eq!(second.available_tracks, 1);
        assert_eq!(second.unsupported_tracks, 0);
        assert_eq!(second.records_inserted, 0);
        assert_eq!(second.records_skipped, 1);
        assert_eq!(second.failures, 0);
        assert_eq!(second.last_error, None);
        let stats = repository
            .load_track_pnl_stats(
                &TrackId::new("btc-coin"),
                chrono::NaiveDate::from_ymd_opt(2026, 6, 11).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stats.pnl_asset.as_ref().map(Asset::as_str), Some("BTC"));
        assert_eq!(stats.gross_realized_pnl_cumulative, 0.0);
        assert_eq!(stats.trading_fee_cumulative, 0.0000001610386348);
        assert_eq!(
            stats.net_realized_pnl_cumulative().complete_value(),
            Some(-0.0000001610386348)
        );
        assert_eq!(
            *account.calls.lock().unwrap(),
            vec!["BTC-USD-SWAP".to_string(), "BTC-USD-SWAP".to_string()]
        );
    }

    #[tokio::test]
    async fn backfill_recent_pnl_records_downgrades_account_errors_to_failures() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager(),
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications);
        let (runtime_context, _) = build_runtime_and_effect_worker_test_contexts(
            &services,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            account_monitor,
        );
        let account = FakeAccount::failing("temporary okx outage");

        let summary =
            backfill_recent_pnl_once(&runtime_context.runtime_state().reconcile, &account).await;

        assert_eq!(summary.records_seen, 0);
        assert_eq!(summary.available_tracks, 0);
        assert_eq!(summary.unsupported_tracks, 0);
        assert_eq!(summary.records_inserted, 0);
        assert_eq!(summary.records_skipped, 0);
        assert_eq!(summary.failures, 1);
        assert_eq!(summary.last_error.as_deref(), Some("temporary okx outage"));
        assert_eq!(*account.calls.lock().unwrap(), vec!["BTC-USD-SWAP"]);
        let stats = repository
            .load_track_pnl_stats(
                &TrackId::new("btc-coin"),
                chrono::NaiveDate::from_ymd_opt(2026, 6, 11).unwrap(),
            )
            .await
            .unwrap();
        assert!(stats.is_empty());
    }

    #[tokio::test]
    async fn backfill_recent_pnl_records_distinguishes_supported_empty_from_unsupported() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager(),
            repository.clone() as Arc<dyn TrackMutationStore>,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository.clone() as Arc<dyn TrackEffectJournal>,
            notifications.clone(),
            account_margin_guard,
        );
        let account_monitor = unavailable_account_monitor(notifications);
        let (runtime_context, _) = build_runtime_and_effect_worker_test_contexts(
            &services,
            repository.clone() as Arc<dyn TrackQueryStore>,
            repository as Arc<dyn TrackEffectJournal>,
            account_monitor,
        );

        let available = backfill_recent_pnl_once(
            &runtime_context.runtime_state().reconcile,
            &FakeAccount::new(Vec::new()),
        )
        .await;
        let unsupported = backfill_recent_pnl_once(
            &runtime_context.runtime_state().reconcile,
            &FakeAccount::unsupported(),
        )
        .await;
        let incomplete = backfill_recent_pnl_once(
            &runtime_context.runtime_state().reconcile,
            &FakeAccount::incomplete(Vec::new()),
        )
        .await;

        assert_eq!(available.available_tracks, 1);
        assert_eq!(available.unsupported_tracks, 0);
        assert_eq!(available.incomplete_tracks, 0);
        assert_eq!(available.failures, 0);
        assert_eq!(unsupported.available_tracks, 0);
        assert_eq!(unsupported.unsupported_tracks, 1);
        assert_eq!(unsupported.failures, 0);
        assert_eq!(incomplete.available_tracks, 1);
        assert_eq!(incomplete.incomplete_tracks, 1);
        assert_eq!(incomplete.failures, 0);
    }

    fn test_manager() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(SystemClock));
        manager
            .add_track(
                TrackDefinition::try_new(
                    TrackId::new("btc-coin"),
                    Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
                    TrackConfig {
                        lower_price: 55_000.0,
                        upper_price: 70_000.0,
                        long_exposure_units: 4.0,
                        short_exposure_units: 4.0,
                        notional_per_unit: 300.0,
                        min_rebalance_units: 0.25,
                        shape_family: ShapeFamily::Linear,
                        out_of_band_policy: BandProtectionPolicy::Freeze,
                        risk_acquisition: Default::default(),
                    },
                    Some(2_000.0),
                    LossLimits {
                        daily_loss_limit: 0.001,
                        total_loss_limit: 0.002,
                    },
                    None,
                )
                .unwrap(),
                inverse_instrument_spec(),
            )
            .unwrap();
        manager
    }

    fn inverse_instrument_spec() -> InstrumentSpec {
        let usd = Asset::try_new("USD").unwrap();
        InstrumentSpec::try_inverse(
            Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
            Asset::try_new("BTC").unwrap(),
            usd.clone(),
            Asset::try_new("BTC").unwrap(),
            Amount::new(100.0, usd.clone()).unwrap(),
            Default::default(),
            0.1,
            1.0,
            1.0,
            Amount::new(0.0, usd).unwrap(),
        )
        .unwrap()
    }

    struct FakeAccount {
        response: FakeAccountResponse,
        calls: Mutex<Vec<String>>,
    }

    enum FakeAccountResponse {
        Records(Vec<TrackPnlRecord>),
        Incomplete(Vec<TrackPnlRecord>),
        Unsupported,
        Error(String),
    }

    impl FakeAccount {
        fn new(records: Vec<TrackPnlRecord>) -> Self {
            Self {
                response: FakeAccountResponse::Records(records),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                response: FakeAccountResponse::Error(message.to_string()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn incomplete(records: Vec<TrackPnlRecord>) -> Self {
            Self {
                response: FakeAccountResponse::Incomplete(records),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn unsupported() -> Self {
            Self {
                response: FakeAccountResponse::Unsupported,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeAccount {
        async fn get_recent_track_pnl_records(
            &self,
            instrument: &Instrument,
            required_window: &poise_application::exchange::PnlHistoryWindow,
        ) -> Result<RecentTrackPnlRecords> {
            self.calls.lock().unwrap().push(instrument.symbol.clone());
            match &self.response {
                FakeAccountResponse::Records(records) => Ok(RecentTrackPnlRecords::Available(
                    poise_application::exchange::RecentTrackPnlBatch::complete(
                        required_window.clone(),
                        records.clone(),
                    ),
                )),
                FakeAccountResponse::Incomplete(records) => Ok(RecentTrackPnlRecords::Available(
                    poise_application::exchange::RecentTrackPnlBatch::incomplete(
                        None,
                        records.clone(),
                    ),
                )),
                FakeAccountResponse::Unsupported => Ok(RecentTrackPnlRecords::Unsupported),
                FakeAccountResponse::Error(message) => anyhow::bail!(message.clone()),
            }
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
