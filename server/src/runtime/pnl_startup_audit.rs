use anyhow::{Result, bail};
use poise_application::TrackInput;
use poise_core::events::DomainEvent;
use poise_core::track::TrackId;

use crate::pnl_audit::{RecentFillCoverage, RecentFillsAuditError, RecentPnlAvailability};
use crate::server_context::RuntimeState;

const MISSING_SOURCE_KEY_SAMPLE_LIMIT: usize = 5;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct StartupPnlAuditSummary {
    pub tracks_audited: usize,
    pub missing_records: usize,
    pub diagnostics_written: usize,
    pub failures: usize,
}

pub(super) async fn run_startup_pnl_audit(state: &RuntimeState) -> Result<StartupPnlAuditSummary> {
    let tracks = state.reconcile.runtime.track_instruments().await;
    let mut summary = StartupPnlAuditSummary::default();
    let mut available_audits = Vec::new();

    for track in tracks {
        let track_id = TrackId::new(&track.id);
        let audit = match state
            .recent_fills_auditor
            .audit_recent_fills(&track_id, &track.instrument)
            .await
        {
            Ok(audit) => audit,
            Err(RecentFillsAuditError::Exchange(error)) => {
                summary.failures += 1;
                state.pnl_backfill_status.record_error_now(format!(
                    "startup pnl audit failed for track `{}`: {error}",
                    track.id
                ));
                tracing::warn!("startup pnl audit failed for track `{}`: {error}", track.id);
                continue;
            }
            Err(error @ RecentFillsAuditError::Store(_)) => {
                state.pnl_backfill_status.record_error_now(format!(
                    "startup pnl audit failed for track `{}`: {error}",
                    track.id
                ));
                return Err(anyhow::anyhow!(
                    "startup pnl audit failed for track `{}`: {error}",
                    track.id
                ));
            }
        };

        if audit.availability == RecentPnlAvailability::Unsupported {
            let error = format!(
                "required recent PNL recovery is unsupported for track `{}` instrument `{}:{}`",
                track.id,
                track.instrument.venue.as_str(),
                track.instrument.symbol
            );
            state.pnl_backfill_status.record_error_now(&error);
            bail!(error);
        }

        summary.tracks_audited += 1;
        available_audits.push((track.id, audit));
    }

    for (track_id, audit) in available_audits {
        if audit.records_missing == 0 {
            continue;
        }

        let sample_source_keys = audit
            .items
            .iter()
            .filter(|item| item.coverage == RecentFillCoverage::Missing)
            .filter_map(|item| item.record.source_key.clone())
            .take(MISSING_SOURCE_KEY_SAMPLE_LIMIT)
            .collect::<Vec<_>>();
        let event = DomainEvent::PnlAuditMissingRecords {
            missing_count: audit.records_missing,
            sample_source_keys,
        };
        match state
            .reconcile
            .runtime
            .handle(
                &TrackId::new(&track_id),
                TrackInput::DurableEvents(vec![event]),
            )
            .await
        {
            Ok(_) => {
                summary.missing_records += audit.records_missing;
                summary.diagnostics_written += 1;
            }
            Err(error) => {
                summary.failures += 1;
                state.pnl_backfill_status.record_error_now(format!(
                    "startup pnl audit diagnostic write failed for track `{}`: {error}",
                    track_id
                ));
                tracing::warn!(
                    "startup pnl audit diagnostic write failed for track `{}`: {error}",
                    track_id
                );
            }
        }
    }

    if summary.failures == 0 {
        state.pnl_backfill_status.record_success_now();
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use poise_application::{TrackEffectJournal, TrackMutationStore, TrackQueryStore};
    use poise_core::events::DomainEvent;
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::Side;
    use poise_engine::ledger::TrackPnlRecord;
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        AccountCapacitySnapshot, AccountPort, RecentTrackPnlRecords, UserDataEvent,
    };
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use crate::assembly::SystemClock;
    use crate::pnl_audit::RecentFillsAuditor;
    use crate::test_support::{
        RecordingExecutionPort, build_runtime_and_effect_worker_test_contexts,
        build_test_application_services, unavailable_account_monitor,
    };

    use super::run_startup_pnl_audit;

    #[tokio::test]
    async fn startup_pnl_audit_records_missing_recent_fill_diagnostic_event() {
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
        let mut state = runtime_context.runtime_state();
        state.recent_fills_auditor = Arc::new(RecentFillsAuditor::new(
            Arc::new(FakeRecentFillsAccount::new(
                FakeRecentFillsResponse::Available(vec![TrackPnlRecord::trade(
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap(),
                    "okx:fills".to_string(),
                    Some("okx:fills:missing".to_string()),
                    Some("order-1".to_string()),
                    Some("missing".to_string()),
                    Side::Sell,
                    62_000.0,
                    0.2,
                    0.0001,
                    0.00001,
                    "USDT",
                )]),
            )),
            repository.clone() as Arc<dyn TrackQueryStore>,
        ));

        let summary = run_startup_pnl_audit(&state).await.unwrap();

        assert_eq!(summary.tracks_audited, 1);
        assert_eq!(summary.missing_records, 1);
        assert_eq!(summary.diagnostics_written, 1);
        let events = repository
            .list_recent_track_events(&TrackId::new("btc-core"), 20)
            .await
            .unwrap();
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                DomainEvent::PnlAuditMissingRecords {
                    missing_count: 1,
                    sample_source_keys,
                } if sample_source_keys == &vec!["okx:fills:missing".to_string()]
            )
        }));
    }

    #[tokio::test]
    async fn startup_pnl_audit_rejects_unsupported_required_history() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let mut state = test_runtime_state(repository.clone()).await;
        state.recent_fills_auditor = Arc::new(RecentFillsAuditor::new(
            Arc::new(FakeRecentFillsAccount::new(
                FakeRecentFillsResponse::Unsupported,
            )),
            repository as Arc<dyn TrackQueryStore>,
        ));

        let error = run_startup_pnl_audit(&state).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("required recent PNL recovery is unsupported")
        );
        assert!(error.to_string().contains("btc-core"));
        assert!(error.to_string().contains("binance:BTCUSDT"));
    }

    #[tokio::test]
    async fn startup_pnl_audit_keeps_transient_query_failure_degraded_but_nonfatal() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let mut state = test_runtime_state(repository.clone()).await;
        state.recent_fills_auditor = Arc::new(RecentFillsAuditor::new(
            Arc::new(FakeRecentFillsAccount::new(FakeRecentFillsResponse::Error(
                "temporary exchange outage".to_string(),
            ))),
            repository as Arc<dyn TrackQueryStore>,
        ));

        let summary = run_startup_pnl_audit(&state).await.unwrap();

        assert_eq!(summary.failures, 1);
        assert_eq!(summary.tracks_audited, 0);
        let health = state.pnl_backfill_status.snapshot();
        assert_eq!(
            health.health_status(),
            crate::runtime::RuntimeHealthStatus::Degraded
        );
        assert!(
            health
                .last_error
                .unwrap()
                .contains("temporary exchange outage")
        );
    }

    #[tokio::test]
    async fn unsupported_history_stops_runtime_before_exchange_cleanup() {
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
        let (runtime_context, effect_worker_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                repository.clone() as Arc<dyn TrackEffectJournal>,
                account_monitor,
            );
        let account = Arc::new(FakeRecentFillsAccount::new(
            FakeRecentFillsResponse::Unsupported,
        ));
        let mut state = runtime_context.runtime_state();
        state.recent_fills_auditor = Arc::new(RecentFillsAuditor::new(
            account.clone(),
            repository as Arc<dyn TrackQueryStore>,
        ));
        let execution = Arc::new(RecordingExecutionPort::default());
        let unexpected = Arc::new(UnexpectedStartupPort);
        let runtime = crate::runtime::ServerRuntime::new(
            state,
            effect_worker_context.effect_worker_state,
            crate::runtime::RuntimePorts::new(
                execution.clone(),
                unexpected.clone(),
                unexpected.clone(),
                account.clone(),
                unexpected,
                Arc::new(SystemClock),
            ),
            vec![crate::runtime::RuntimeStartupDefinition::new(
                test_track(),
                1,
            )],
        );

        let error = runtime.start().await.err().unwrap();

        assert!(
            error
                .to_string()
                .contains("required recent PNL recovery is unsupported")
        );
        assert_eq!(execution.cancel_all_call_count(), 0);
        assert_eq!(account.subscribe_call_count(), 0);
    }

    async fn test_runtime_state(
        repository: Arc<SqliteStorage>,
    ) -> crate::server_context::RuntimeState {
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
        runtime_context.runtime_state()
    }

    fn test_manager() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(SystemClock));
        manager
            .add_track(test_track(), test_instrument_spec())
            .unwrap();
        manager
    }

    fn test_track() -> TrackDefinition {
        TrackDefinition::try_new(
            TrackId::new("btc-core"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
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
                daily_loss_limit: 100.0,
                total_loss_limit: 300.0,
            },
            None,
        )
        .unwrap()
    }

    fn test_instrument_spec() -> poise_core::instrument::InstrumentSpec {
        crate::test_support::test_instrument_spec_with(
            Instrument::new(Venue::Binance, "BTCUSDT"),
            0.1,
            0.001,
            0.001,
            5.0,
        )
    }

    struct FakeRecentFillsAccount {
        response: FakeRecentFillsResponse,
        subscribe_calls: AtomicUsize,
    }

    impl FakeRecentFillsAccount {
        fn new(response: FakeRecentFillsResponse) -> Self {
            Self {
                response,
                subscribe_calls: AtomicUsize::new(0),
            }
        }

        fn subscribe_call_count(&self) -> usize {
            self.subscribe_calls.load(Ordering::SeqCst)
        }
    }

    enum FakeRecentFillsResponse {
        Available(Vec<TrackPnlRecord>),
        Unsupported,
        Error(String),
    }

    struct UnexpectedStartupPort;

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountSummaryPort for UnexpectedStartupPort {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            anyhow::bail!("account summary should not run before PNL capability gate")
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::MarketDataPort for UnexpectedStartupPort {
        async fn subscribe_prices(
            &self,
            _instrument: &Instrument,
        ) -> Result<mpsc::Receiver<poise_engine::ports::MarketDataTick>> {
            anyhow::bail!("market data should not run before PNL capability gate")
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::MetadataPort for UnexpectedStartupPort {
        async fn get_exchange_info(
            &self,
            _instrument: &Instrument,
        ) -> Result<poise_engine::ports::ExchangeInfo> {
            anyhow::bail!("metadata should not run before PNL capability gate")
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            anyhow::bail!("server time should not run before PNL capability gate")
        }
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeRecentFillsAccount {
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
            _instrument: &Instrument,
        ) -> Result<RecentTrackPnlRecords> {
            match &self.response {
                FakeRecentFillsResponse::Available(records) => {
                    Ok(RecentTrackPnlRecords::Available(records.clone()))
                }
                FakeRecentFillsResponse::Unsupported => Ok(RecentTrackPnlRecords::Unsupported),
                FakeRecentFillsResponse::Error(message) => anyhow::bail!(message.clone()),
            }
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            self.subscribe_calls.fetch_add(1, Ordering::SeqCst);
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
