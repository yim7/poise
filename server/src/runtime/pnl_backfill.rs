use std::sync::Arc;
use std::time::Duration;

use poise_engine::ports::AccountPort;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::server_context::ReconcileState;

use super::ServerRuntime;

const PNL_BACKFILL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PnlBackfillSummary {
    pub records_seen: usize,
    pub records_inserted: usize,
    pub failures: usize,
}

pub(super) fn spawn_pnl_backfill_task(
    runtime: &ServerRuntime,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.reconcile.clone();
    let account = Arc::clone(&runtime.account);

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
                    if summary.records_inserted > 0 {
                        tracing::info!(
                            records_seen = summary.records_seen,
                            records_inserted = summary.records_inserted,
                            failures = summary.failures,
                            "track pnl backfill inserted records"
                        );
                    } else if summary.failures > 0 {
                        tracing::warn!(
                            records_seen = summary.records_seen,
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
    let tracks = state.observation_service.track_instruments().await;
    let mut summary = PnlBackfillSummary::default();

    for track in tracks {
        let records = match account
            .get_recent_track_pnl_records(&track.instrument)
            .await
        {
            Ok(records) => records,
            Err(error) => {
                summary.failures += 1;
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
                state
                    .observation_service
                    .resolve_track_id(&record.instrument)
                    .await
            };
            let Some(track_id) = track_id else {
                tracing::warn!(
                    venue = record.instrument.venue.as_str(),
                    symbol = %record.instrument.symbol,
                    source_key = ?record.source_key,
                    "received backfilled pnl record for unknown track instrument"
                );
                continue;
            };

            match state
                .observation_service
                .record_track_pnl(&track_id, record)
                .await
            {
                Ok(true) => summary.records_inserted += 1,
                Ok(false) => {}
                Err(error) => {
                    summary.failures += 1;
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
    use poise_application::{TrackEffectJournal, TrackMutationStore, TrackQueryStore};
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::{ExchangeRules, Side};
    use poise_engine::ledger::TrackPnlRecord;
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{AccountCapacitySnapshot, AccountPort, UserDataEvent};
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use crate::assembly::SystemClock;
    use crate::test_support::{
        build_runtime_and_effect_worker_test_contexts, build_test_application_services,
        unavailable_account_monitor,
    };

    use super::backfill_recent_pnl_once;

    #[tokio::test]
    async fn backfill_recent_pnl_records_persists_new_records_idempotently() {
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
        let account = FakeAccount::new(vec![TrackPnlRecord::trade(
            Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
            Utc.with_ymd_and_hms(2026, 6, 11, 2, 16, 1).unwrap(),
            "okx:fills".to_string(),
            Some("okx:orders:btc-usd-swap:460607985".to_string()),
            Some("3645412589617586176".to_string()),
            Some("460607985".to_string()),
            Side::Sell,
            62_096.9,
            0.2,
            0.0,
            0.0000001610386348,
            "BTC",
        )]);

        let first =
            backfill_recent_pnl_once(&runtime_context.runtime_state().reconcile, &account).await;
        let second =
            backfill_recent_pnl_once(&runtime_context.runtime_state().reconcile, &account).await;

        assert_eq!(first.records_seen, 1);
        assert_eq!(first.records_inserted, 1);
        assert_eq!(second.records_seen, 1);
        assert_eq!(second.records_inserted, 0);
        let stats = repository
            .load_track_pnl_stats(
                &TrackId::new("btc-coin"),
                chrono::NaiveDate::from_ymd_opt(2026, 6, 11).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stats.pnl_asset.as_deref(), Some("BTC"));
        assert_eq!(stats.gross_realized_pnl_cumulative, 0.0);
        assert_eq!(stats.trading_fee_cumulative, 0.0000001610386348);
        assert_eq!(stats.net_realized_pnl_cumulative(), -0.0000001610386348);
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
        assert_eq!(summary.records_inserted, 0);
        assert_eq!(summary.failures, 1);
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
                ExchangeRules {
                    price_tick: 0.1,
                    price_precision: Default::default(),
                    quantity_kind: poise_core::types::QuantityKind::InverseContract,
                    contract_notional: Some(100.0),
                    settlement_asset: "BTC".to_string(),
                    quantity_step: 1.0,
                    min_qty: 1.0,
                    min_notional: 0.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            )
            .unwrap();
        manager
    }

    struct FakeAccount {
        response: FakeAccountResponse,
        calls: Mutex<Vec<String>>,
    }

    enum FakeAccountResponse {
        Records(Vec<TrackPnlRecord>),
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
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeAccount {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> Result<AccountCapacitySnapshot> {
            Ok(AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn get_recent_track_pnl_records(
            &self,
            instrument: &Instrument,
        ) -> Result<Vec<TrackPnlRecord>> {
            self.calls.lock().unwrap().push(instrument.symbol.clone());
            match &self.response {
                FakeAccountResponse::Records(records) => Ok(records.clone()),
                FakeAccountResponse::Error(message) => anyhow::bail!(message.clone()),
            }
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
