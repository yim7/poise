use std::sync::Arc;

use anyhow::Result;
use poise_engine::track::TrackId;

use crate::mutation_executor::MutationExecutor;
use crate::{
    TrackObservationService, TrackQueryStore, TrackRecoveryIssue, runtime_read_state_loader,
};

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

    pub async fn restore_persisted_track_state(&self, id: &str) -> Result<bool> {
        self.executor.restore_persisted_track_state(id).await
    }

    pub async fn load_track_recovery_summary(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackRecoverySummary>> {
        let Some(runtime) = runtime_read_state_loader::load_runtime_read_state(
            self.query_store.clone(),
            Some(self.observation.clone()),
            track_id,
        )
        .await?
        else {
            return Ok(None);
        };

        Ok(Some(TrackRecoverySummary {
            issue: runtime
                .executor_state
                .diagnostics
                .recovery_anomaly
                .clone()
                .map(TrackRecoveryIssue::from),
            has_working_orders: runtime
                .executor_state
                .slots
                .iter()
                .any(|slot| slot.working_order.is_some()),
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

    use chrono::{TimeZone, Utc};
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{Exposure, Side};
    use poise_engine::executor::{ExecutionMode, OrderRole, OrderSlot, RecoveryAnomaly};
    use poise_engine::persisted_runtime::TrackRestoreRevision;
    use poise_engine::ports::OrderStatus;
    use poise_engine::runtime::{
        AutoState, ControlState, ExecutionSlot, ExecutionStats, ExecutorState, RiskState,
        SlotState, TrackState, WorkingOrder,
    };
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};

    use crate::mutation_executor::test_support::{
        MemoryRepository, seeded_manager, track_write_services,
    };
    use crate::{TrackRecoveryIssue, runtime_read_state_loader};

    #[tokio::test]
    async fn load_track_recovery_summary_projects_application_owned_issue() {
        let write_repository = Arc::new(MemoryRepository::default());
        let mut snapshot = test_snapshot();
        snapshot.executor_state.diagnostics.recovery_anomaly =
            Some(RecoveryAnomaly::UnknownLiveOrder);
        snapshot.executor_state.slots.clear();
        crate::TrackMutationStore::save_transition(
            write_repository.as_ref(),
            "btc-core",
            &snapshot,
            &[],
            &[],
        )
        .await
        .unwrap();
        let (services, _) = track_write_services(seeded_manager(), write_repository);
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
    async fn restore_persisted_track_state_rehydrates_manager_from_store() {
        let repository = Arc::new(MemoryRepository::default());
        let mut persisted_manager = seeded_manager();
        persisted_manager.pause_track("btc-core").unwrap();
        let persisted_snapshot = persisted_manager.snapshot("btc-core").unwrap();
        crate::TrackMutationStore::save_transition(
            repository.as_ref(),
            "btc-core",
            &persisted_snapshot,
            &[],
            &[],
        )
        .await
        .unwrap();

        let (services, _) = track_write_services(seeded_manager(), repository);

        assert!(
            services
                .runtime_lifecycle
                .restore_persisted_track_state("btc-core")
                .await
                .unwrap()
        );
        assert_eq!(
            services
                .runtime_lifecycle
                .manager()
                .read()
                .await
                .snapshot("btc-core")
                .unwrap()
                .status(),
            poise_engine::runtime::TrackStatus::Paused
        );
    }

    #[tokio::test]
    async fn runtime_read_state_loader_merges_snapshot_and_live_view_once() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());
        services
            .observation
            .observe_market(
                "btc-core",
                poise_engine::observation::MarketObservation {
                    mark_price: 95.0,
                    execution_quote: Some(poise_engine::ports::ExecutionQuote {
                        best_bid: 94.5,
                        best_ask: 95.5,
                    }),
                },
            )
            .await
            .unwrap();

        let runtime = runtime_read_state_loader::load_runtime_read_state(
            repository.clone(),
            Some(Arc::new(services.observation.clone())),
            &TrackId::new("btc-core"),
        )
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

    fn test_snapshot() -> TrackRuntimeSnapshot {
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
        TrackRuntimeSnapshot {
            track_id: TrackId::new("btc-core"),
            restore_revision: TrackRestoreRevision::for_track(
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                &track_config,
            ),
            runtime_state: TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)),
            current_exposure: Exposure(3.5),
            desired_exposure: Some(Exposure(4.0)),
            executor_state: ExecutorState {
                active_round: Some(poise_engine::runtime::ExecutionRound {
                    desired_exposure: Exposure(4.0),
                    mode: ExecutionMode::Passive,
                    started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
                }),
                diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                    mode: ExecutionMode::Passive,
                    inventory_gap: Exposure(0.5),
                    gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 26, 10, 0, 0).unwrap()),
                    last_reprice_at: None,
                    last_execution_reason: None,
                    recovery_anomaly: None,
                },
                slots: vec![ExecutionSlot {
                    slot: OrderSlot::new("inventory_core"),
                    state: SlotState::Working,
                    working_order: Some(WorkingOrder {
                        order_id: Some("order-1".into()),
                        client_order_id: "client-1".into(),
                        side: Side::Buy,
                        price: 100.5,
                        quantity: 0.1,
                        status: OrderStatus::New,
                        role: OrderRole::IncreaseInventory,
                    }),
                }],
                recent_terminal_orders: Vec::new(),
                stats: ExecutionStats {
                    started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
                    max_inventory_gap_abs: Exposure(0.5),
                    max_gap_age_ms: 0,
                },
            },
            ledger_state: Default::default(),
            replacement_gate_reason: None,
            execution_gate_state: poise_engine::execution_gate::ExecutionGateState::open(),
            risk: RiskState {
                unrealized_pnl: 0.0,
                ..RiskState::default()
            },
            observed: ObservedState::default(),
        }
    }
}
