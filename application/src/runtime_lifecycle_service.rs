use std::sync::Arc;

use anyhow::Result;
use chrono::NaiveDate;
use poise_engine::ledger::TrackLedgerState;
use poise_engine::runtime::FreshSessionExternalInputs;
use poise_engine::track::TrackId;

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

    pub async fn load_track_ledger_state(
        &self,
        track_id: &TrackId,
        current_utc_day: NaiveDate,
    ) -> Result<Option<TrackLedgerState>> {
        let Some(mut ledger_state) = self.query_store.load_track_ledger_state(track_id).await?
        else {
            return Ok(None);
        };
        ledger_state.normalize_utc_day(current_utc_day);
        Ok(Some(ledger_state))
    }

    async fn load_persisted_components(
        &self,
        track_id: &TrackId,
        current_utc_day: NaiveDate,
    ) -> Result<(TrackControlState, TrackLedgerState)> {
        let control_state = self
            .load_track_control_state(track_id)
            .await?
            .unwrap_or_default();
        let ledger_state = self
            .load_track_ledger_state(track_id, current_utc_day)
            .await?
            .unwrap_or_else(|| {
                let mut ledger_state = TrackLedgerState::default();
                ledger_state.normalize_utc_day(current_utc_day);
                ledger_state
            });
        Ok((control_state, ledger_state))
    }

    pub async fn fresh_start_track_runtime(
        &self,
        track_id: &TrackId,
        current_utc_day: NaiveDate,
        external_inputs: FreshSessionExternalInputs,
    ) -> Result<bool> {
        let (control_state, ledger_state) = self
            .load_persisted_components(track_id, current_utc_day)
            .await?;
        self.executor
            .fresh_start_track_runtime(track_id, control_state, ledger_state, external_inputs)
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
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::Exposure;
    use poise_engine::executor::RecoveryAnomaly;
    use poise_engine::ledger::TrackLedgerState;
    use poise_engine::mutation_frame::TrackMutationFrameRevision;
    use poise_engine::mutation_frame::{FrameObservedState, TrackMutationFrame};
    use poise_engine::ports::ExecutionQuote;
    use poise_engine::runtime::{
        AutoState, ControlState, CurrentMarketData, ExecutorState, FreshSessionExternalInputs,
        RiskState, TrackState,
    };
    use poise_engine::track::{Instrument, TrackId, Venue};

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
        let mut snapshot = test_snapshot();
        snapshot.executor_state.recovery_anomaly = Some(RecoveryAnomaly::UnknownLiveOrder);
        snapshot.executor_state.bindings.clear();
        let mut manager = seeded_manager();
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
    async fn load_track_ledger_state_normalizes_utc_day() {
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
        TrackMutationStore::save_track_ledger_state(
            repository.as_ref(),
            &TrackId::new("btc-core"),
            &TrackLedgerState {
                ledger_utc_day: NaiveDate::from_ymd_opt(2026, 4, 22).unwrap(),
                gross_realized_pnl_today: 25.0,
                gross_realized_pnl_cumulative: 100.0,
                trading_fee_today: 2.0,
                trading_fee_cumulative: 8.0,
                funding_fee_today: -1.0,
                funding_fee_cumulative: -4.0,
                unresolved_gaps: vec![],
            },
        )
        .await
        .unwrap();
        let (services, _) = track_write_services(seeded_manager(), repository);

        let ledger_state = services
            .runtime_lifecycle
            .load_track_ledger_state(
                &TrackId::new("btc-core"),
                NaiveDate::from_ymd_opt(2026, 4, 23).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        let control_state = services
            .runtime_lifecycle
            .load_track_control_state(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ledger_state.ledger_utc_day,
            NaiveDate::from_ymd_opt(2026, 4, 23).unwrap()
        );
        assert_eq!(ledger_state.gross_realized_pnl_today, 0.0);
        assert_eq!(ledger_state.trading_fee_today, 0.0);
        assert_eq!(ledger_state.funding_fee_today, 0.0);
        assert_eq!(ledger_state.gross_realized_pnl_cumulative, 100.0);
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
        TrackMutationStore::save_track_ledger_state(
            repository.as_ref(),
            &TrackId::new("btc-core"),
            &TrackLedgerState {
                gross_realized_pnl_cumulative: 42.0,
                ..TrackLedgerState::default()
            },
        )
        .await
        .unwrap();

        let (services, _) = track_write_services(seeded_manager(), repository.clone());
        {
            let manager_handle = services.runtime_lifecycle.manager();
            let mut manager = manager_handle.write().await;
            let mut snapshot = manager.mutation_frame("btc-core").unwrap();
            snapshot.current_exposure = Exposure(9.0);
            snapshot.desired_exposure = Some(Exposure(4.0));
            snapshot.executor_state.recovery_anomaly = Some(RecoveryAnomaly::UnknownLiveOrder);
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
        assert_eq!(snapshot.current_exposure, Exposure(1.5));
        assert_eq!(snapshot.desired_exposure, None);
        assert!(snapshot.executor_state.bindings.is_empty());
        assert!(snapshot.executor_state.recovery_anomaly.is_none());
        assert_eq!(snapshot.ledger_state.gross_realized_pnl_cumulative, 42.0);
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
            snapshot.executor_state.recovery_anomaly = Some(RecoveryAnomaly::UnknownLiveOrder);
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
            snapshot.executor_state.recovery_anomaly = Some(RecoveryAnomaly::UnknownLiveOrder);
            assert!(!snapshot.executor_state.bindings.is_empty());
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
        assert!(snapshot.executor_state.bindings.is_empty());
        assert!(snapshot.executor_state.recovery_anomaly.is_none());
        assert_eq!(
            snapshot.executor_state.ledger_state.ledger_anchor_exposure,
            Exposure(0.0)
        );
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

    fn test_snapshot() -> TrackMutationFrame {
        let track_config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        };
        TrackMutationFrame {
            track_id: TrackId::new("btc-core"),
            frame_revision: TrackMutationFrameRevision::for_track(
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                &track_config,
            ),
            runtime_state: TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)),
            current_exposure: Exposure(3.5),
            desired_exposure: Some(Exposure(4.0)),
            executor_state: ExecutorState::empty(
                Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
            ),
            ledger_state: Default::default(),
            execution_gate_state: poise_engine::execution_gate::ExecutionGateState::open(),
            risk: RiskState {
                unrealized_pnl: 0.0,
                ..RiskState::default()
            },
            observed: FrameObservedState::default(),
        }
    }
}
