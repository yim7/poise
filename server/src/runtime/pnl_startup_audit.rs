use poise_core::events::DomainEvent;
use poise_core::track::TrackId;

use crate::pnl_audit::RecentFillCoverage;
use crate::server_context::RuntimeState;

const MISSING_SOURCE_KEY_SAMPLE_LIMIT: usize = 5;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct StartupPnlAuditSummary {
    pub tracks_audited: usize,
    pub missing_records: usize,
    pub diagnostics_written: usize,
    pub failures: usize,
}

pub(super) async fn run_startup_pnl_audit(state: &RuntimeState) -> StartupPnlAuditSummary {
    let tracks = state
        .reconcile
        .observation_service
        .track_instruments()
        .await;
    let mut summary = StartupPnlAuditSummary::default();

    for track in tracks {
        let track_id = TrackId::new(&track.id);
        let audit = match state
            .recent_fills_auditor
            .audit_recent_fills(&track_id, &track.instrument)
            .await
        {
            Ok(audit) => audit,
            Err(error) => {
                summary.failures += 1;
                state.runtime_health.record_error_now(
                    super::RuntimeHealthComponent::PnlBackfill,
                    format!("startup pnl audit failed for track `{}`: {error}", track.id),
                );
                tracing::warn!("startup pnl audit failed for track `{}`: {error}", track.id);
                continue;
            }
        };

        summary.tracks_audited += 1;
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
            .observation_service
            .record_track_events(&track.id, &[event])
            .await
        {
            Ok(()) => {
                summary.missing_records += audit.records_missing;
                summary.diagnostics_written += 1;
            }
            Err(error) => {
                summary.failures += 1;
                state.runtime_health.record_error_now(
                    super::RuntimeHealthComponent::PnlBackfill,
                    format!(
                        "startup pnl audit diagnostic write failed for track `{}`: {error}",
                        track.id
                    ),
                );
                tracing::warn!(
                    "startup pnl audit diagnostic write failed for track `{}`: {error}",
                    track.id
                );
            }
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use poise_application::{TrackEffectJournal, TrackMutationStore, TrackQueryStore};
    use poise_core::events::DomainEvent;
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
    use crate::pnl_audit::RecentFillsAuditor;
    use crate::test_support::{
        build_runtime_and_effect_worker_test_contexts, build_test_application_services,
        unavailable_account_monitor,
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
            Arc::new(FakeRecentFillsAccount {
                records: vec![TrackPnlRecord::trade(
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
                )],
            }),
            repository.clone() as Arc<dyn TrackQueryStore>,
        ));

        let summary = run_startup_pnl_audit(&state).await;

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

    fn test_manager() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(SystemClock));
        manager
            .add_track(test_track(), test_exchange_rules())
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

    fn test_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: Default::default(),
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
            quantity_step: 0.001,
            min_qty: 0.001,
            min_notional: 5.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    struct FakeRecentFillsAccount {
        records: Vec<TrackPnlRecord>,
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
        ) -> Result<Vec<TrackPnlRecord>> {
            Ok(self.records.clone())
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
