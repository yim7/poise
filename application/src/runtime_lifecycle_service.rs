use std::sync::Arc;

use anyhow::Result;
use chrono::NaiveDate;
use poise_core::track::TrackId;
use poise_engine::ledger::TrackPnlStats;
use poise_engine::runtime::FreshSessionExternalInputs;

use crate::mutation_executor::MutationExecutor;
use crate::{TrackControlState, TrackObservationService, TrackQueryStore, TrackRecoveryIssue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRecoverySummary {
    pub issue: Option<TrackRecoveryIssue>,
    pub has_working_orders: bool,
}

#[derive(Clone)]
pub struct TrackRuntimeLifecycleService {
    executor: Arc<MutationExecutor>,
    query_store: Arc<dyn TrackQueryStore>,
    observation: Arc<TrackObservationService>,
}

impl TrackRuntimeLifecycleService {
    pub(crate) fn from_executor(
        executor: Arc<MutationExecutor>,
        query_store: Arc<dyn TrackQueryStore>,
        observation: Arc<TrackObservationService>,
    ) -> Self {
        Self {
            executor,
            query_store,
            observation,
        }
    }

    pub async fn prepare_fresh_session_for_activation(&self, id: &str) -> Result<()> {
        self.executor.prepare_fresh_session_for_activation(id).await
    }

    pub async fn load_track_control_state(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackControlState>> {
        self.query_store.load_track_control_state(track_id).await
    }

    pub async fn load_track_pnl_stats(
        &self,
        track_id: &TrackId,
        current_utc_day: NaiveDate,
    ) -> Result<TrackPnlStats> {
        self.query_store
            .load_track_pnl_stats(track_id, current_utc_day)
            .await
    }

    async fn load_persisted_components(
        &self,
        track_id: &TrackId,
        current_utc_day: NaiveDate,
    ) -> Result<(TrackControlState, TrackPnlStats)> {
        let control_state = self
            .load_track_control_state(track_id)
            .await?
            .unwrap_or_default();
        let pnl_stats = self.load_track_pnl_stats(track_id, current_utc_day).await?;
        Ok((control_state, pnl_stats))
    }

    pub async fn fresh_start_track_runtime(
        &self,
        track_id: &TrackId,
        current_utc_day: NaiveDate,
        external_inputs: FreshSessionExternalInputs,
    ) -> Result<bool> {
        let (control_state, pnl_stats) = self
            .load_persisted_components(track_id, current_utc_day)
            .await?;
        self.executor
            .fresh_start_track_runtime(track_id, control_state, pnl_stats, external_inputs)
            .await
    }

    pub async fn load_track_recovery_summary(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackRecoverySummary>> {
        let Some(runtime) = self
            .observation
            .track_runtime_view(track_id.as_str())
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(TrackRecoverySummary {
            issue: runtime
                .executor
                .recovery_anomaly
                .clone()
                .map(TrackRecoveryIssue::from),
            has_working_orders: !runtime.executor.bindings.is_empty(),
        }))
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub fn manager(&self) -> Arc<tokio::sync::RwLock<poise_engine::manager::TrackManager>> {
        self.executor.manager()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{NaiveDate, TimeZone, Utc};
    use poise_core::track::TrackId;
    use poise_core::types::Exposure;
    use poise_engine::executor::RecoveryAnomaly;
    use poise_engine::ledger::TrackPnlRecord;
    use poise_engine::ports::ExecutionQuote;
    use poise_engine::runtime::{CurrentMarketData, FreshSessionExternalInputs};

    use crate::mutation_executor::test_support::{
        MemoryRepository, seeded_manager, track_write_services,
    };
    use crate::{
        EffectStatus, PersistedControlMode, TrackControlState, TrackMutationStore,
        TrackRecoveryIssue,
    };

    #[tokio::test]
    async fn load_track_recovery_summary_projects_application_owned_issue() {
        let write_repository = Arc::new(MemoryRepository::default());
        let mut manager = seeded_manager();
        let mut snapshot = manager.mutation_frame("btc-core").unwrap();
        snapshot.set_recovery_anomaly(Some(RecoveryAnomaly::UnknownLiveOrder));
        snapshot.clear_executor_bindings();
        manager.rollback_track_state(&snapshot).unwrap();
        let (services, _) = track_write_services(manager, write_repository);
        let service = services.runtime_lifecycle;

        let summary = service
            .load_track_recovery_summary(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(summary.issue, Some(TrackRecoveryIssue::UnknownLiveOrder));
        assert!(!summary.has_working_orders);
    }

    #[tokio::test]
    async fn load_track_pnl_stats_aggregates_records_for_utc_day() {
        let repository = Arc::new(MemoryRepository::default());
        TrackMutationStore::save_track_control_state(
            repository.as_ref(),
            &TrackId::new("btc-core"),
            &TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            },
        )
        .await
        .unwrap();
        TrackMutationStore::insert_track_pnl_record(
            repository.as_ref(),
            &TrackId::new("btc-core"),
            &TrackPnlRecord::trade_summary(
                poise_core::track::Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
                Utc.with_ymd_and_hms(2026, 4, 22, 8, 0, 0).unwrap(),
                "test".into(),
                None,
                None,
                100.0,
                8.0,
            ),
        )
        .await
        .unwrap();
        TrackMutationStore::insert_track_pnl_record(
            repository.as_ref(),
            &TrackId::new("btc-core"),
            &TrackPnlRecord::funding(
                poise_core::track::Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
                Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap(),
                "test".into(),
                None,
                -4.0,
            ),
        )
        .await
        .unwrap();
        let (services, _) = track_write_services(seeded_manager(), repository);

        let pnl_stats = services
            .runtime_lifecycle
            .load_track_pnl_stats(
                &TrackId::new("btc-core"),
                NaiveDate::from_ymd_opt(2026, 4, 23).unwrap(),
            )
            .await
            .unwrap();
        let control_state = services
            .runtime_lifecycle
            .load_track_control_state(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            pnl_stats.pnl_utc_day,
            NaiveDate::from_ymd_opt(2026, 4, 23).unwrap()
        );
        assert_eq!(pnl_stats.gross_realized_pnl_today, 0.0);
        assert_eq!(pnl_stats.trading_fee_today, 0.0);
        assert_eq!(pnl_stats.funding_fee_today, 0.0);
        assert_eq!(pnl_stats.gross_realized_pnl_cumulative, 100.0);
        assert_eq!(
            control_state,
            TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            }
        );
    }

    #[tokio::test]
    async fn fresh_start_track_runtime_rebuilds_manager_from_persistent_state_and_external_inputs()
    {
        let repository = Arc::new(MemoryRepository::default());
        TrackMutationStore::save_track_control_state(
            repository.as_ref(),
            &TrackId::new("btc-core"),
            &TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            },
        )
        .await
        .unwrap();
        TrackMutationStore::insert_track_pnl_record(
            repository.as_ref(),
            &TrackId::new("btc-core"),
            &TrackPnlRecord::trade_summary(
                poise_core::track::Instrument::new(poise_core::track::Venue::Binance, "BTCUSDT"),
                Utc::now(),
                "test".into(),
                None,
                None,
                42.0,
                0.0,
            ),
        )
        .await
        .unwrap();

        let (services, _) = track_write_services(seeded_manager(), repository.clone());
        {
            let manager_handle = services.runtime_lifecycle.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.mutation_frame("btc-core").unwrap();
            snapshot.set_exposure_state(Exposure(9.0), Some(Exposure(4.0)));
            snapshot.set_recovery_anomaly(Some(RecoveryAnomaly::UnknownLiveOrder));
            manager.rollback_track_state(&snapshot).unwrap();
        }

        assert!(
            services
                .runtime_lifecycle
                .fresh_start_track_runtime(
                    &TrackId::new("btc-core"),
                    Utc::now().date_naive(),
                    FreshSessionExternalInputs {
                        current_exposure: Exposure(1.5),
                        position_qty: 1.5,
                        market_data: Some(CurrentMarketData {
                            strategy_price: 96.0,
                            mark_price: Some(95.9),
                            execution_quote: ExecutionQuote {
                                best_bid: 95.8,
                                best_ask: 96.2,
                            },
                            observed_at: Utc.with_ymd_and_hms(2026, 4, 23, 9, 1, 0).unwrap(),
                        }),
                        exchange_rules: poise_core::types::ExchangeRules {
                            price_tick: 0.5,
                            price_precision: Default::default(),
                            quantity_step: 0.001,
                            min_qty: 0.001,
                            min_notional: 5.0,
                            maker_fee_rate: 0.0,
                            taker_fee_rate: 0.0,
                        },
                    },
                )
                .await
                .unwrap()
        );

        let manager = services.runtime_lifecycle.manager();
        let manager = manager.read().await;
        let snapshot = manager.mutation_frame("btc-core").unwrap();
        assert_eq!(
            snapshot.status(),
            poise_engine::runtime::TrackStatus::Paused
        );
        assert_eq!(snapshot.current_exposure(), &Exposure(1.5));
        assert_eq!(snapshot.desired_exposure(), None);
        assert!(!snapshot.has_executor_bindings());
        assert!(!snapshot.recovery_anomaly_active());
        assert_eq!(snapshot.pnl_stats().gross_realized_pnl_cumulative, 42.0);
        let live_view = manager.track_live_view(&TrackId::new("btc-core")).unwrap();
        assert_eq!(live_view.strategy_price, Some(96.0));
        assert_eq!(live_view.best_bid, Some(95.8));
        assert_eq!(live_view.best_ask, Some(96.2));
        let track = manager.get_track("btc-core").unwrap();
        assert_eq!(track.exchange_rules().price_tick, 0.5);
    }

    #[tokio::test]
    async fn track_runtime_view_includes_live_market_fields() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());
        services
            .observation
            .observe_market(
                "btc-core",
                poise_engine::observation::MarketObservation::MarkPrice { mark_price: 95.0 },
            )
            .await
            .unwrap();
        services
            .observation
            .observe_market(
                "btc-core",
                poise_engine::observation::MarketObservation::ExecutionQuote {
                    execution_quote: poise_engine::ports::ExecutionQuote {
                        best_bid: 94.5,
                        best_ask: 95.5,
                    },
                },
            )
            .await
            .unwrap();

        let runtime = services
            .observation
            .track_runtime_view("btc-core")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            runtime.strategy_price_status,
            poise_engine::runtime::StrategyPriceStatus::Live
        );
        assert_eq!(runtime.best_bid, Some(94.5));
        assert_eq!(runtime.best_ask, Some(95.5));
    }

    #[tokio::test]
    async fn load_track_recovery_summary_reads_live_runtime_without_persisted_snapshot() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository);
        {
            let manager_handle = services.runtime_lifecycle.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.mutation_frame("btc-core").unwrap();
            snapshot.set_recovery_anomaly(Some(RecoveryAnomaly::UnknownLiveOrder));
            manager.rollback_track_state(&snapshot).unwrap();
        }

        let summary = services
            .runtime_lifecycle
            .load_track_recovery_summary(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(summary.issue, Some(TrackRecoveryIssue::UnknownLiveOrder));
    }

    #[tokio::test]
    async fn prepare_fresh_session_for_activation_does_not_mutate_old_persisted_effects() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());
        repository.seed_pending_mixed_effect_batch("btc-core", "btc-core:batch-1");
        let _snapshot = services
            .runtime_lifecycle
            .manager()
            .read()
            .await
            .mutation_frame("btc-core")
            .unwrap();
        services
            .observation
            .observe_market(
                "btc-core",
                poise_engine::observation::MarketObservation::MarkPrice { mark_price: 95.0 },
            )
            .await
            .unwrap();
        services
            .observation
            .observe_market(
                "btc-core",
                poise_engine::observation::MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
            )
            .await
            .unwrap();
        {
            let manager_handle = services.runtime_lifecycle.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.mutation_frame("btc-core").unwrap();
            snapshot.set_recovery_anomaly(Some(RecoveryAnomaly::UnknownLiveOrder));
            assert!(snapshot.has_executor_bindings());
            manager.rollback_track_state(&snapshot).unwrap();
        }

        services
            .runtime_lifecycle
            .prepare_fresh_session_for_activation("btc-core")
            .await
            .unwrap();

        let snapshot = services
            .runtime_lifecycle
            .manager()
            .read()
            .await
            .mutation_frame("btc-core")
            .unwrap();
        let effects = repository.pending_effects();
        assert!(!snapshot.has_executor_bindings());
        assert!(!snapshot.recovery_anomaly_active());
        assert_eq!(snapshot.executor_ledger_anchor_exposure(), &Exposure(0.0));
        let btc_statuses = effects
            .iter()
            .filter(|effect| effect.track_id == TrackId::new("btc-core"))
            .filter(|effect| effect.batch_id == "btc-core:batch-1")
            .map(|effect| effect.status)
            .collect::<Vec<_>>();
        assert_eq!(
            btc_statuses,
            vec![EffectStatus::Pending, EffectStatus::Pending],
            "old persisted effects are journal history, not startup work"
        );
    }
}
