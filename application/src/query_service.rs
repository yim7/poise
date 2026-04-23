use std::sync::Arc;

use anyhow::Result;
use poise_engine::track::TrackId;

use crate::{
    PreparedTrackRegistry, TrackListReadModel, TrackObservationService, TrackQueryStore,
    TrackReadModel, track_read_source_loader::TrackReadSourceLoader,
};

pub struct TrackQueryService {
    loader: Arc<TrackReadSourceLoader>,
}

impl TrackQueryService {
    pub(crate) fn from_loader(loader: Arc<TrackReadSourceLoader>) -> Self {
        Self { loader }
    }

    pub fn new(
        repository: Arc<dyn TrackQueryStore>,
        prepared_registry: Arc<PreparedTrackRegistry>,
    ) -> Self {
        Self::new_with_observation(repository, prepared_registry, None)
    }

    pub fn new_with_observation(
        repository: Arc<dyn TrackQueryStore>,
        prepared_registry: Arc<PreparedTrackRegistry>,
        observation: Option<Arc<TrackObservationService>>,
    ) -> Self {
        Self::from_loader(Arc::new(TrackReadSourceLoader::new(
            repository,
            prepared_registry,
            observation,
        )))
    }

    pub async fn list_track_sources(&self) -> Result<Vec<TrackListReadModel>> {
        self.loader.list_track_read_models().await
    }

    pub async fn load_track_detail_source(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackReadModel>> {
        self.loader.load_track_read_model(track_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use poise_core::events::DomainEvent;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{Exposure, Side};
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::observation::MarketObservation;
    use poise_engine::persisted_runtime::TrackRestoreRevision;
    use poise_engine::ports::ExecutionQuote;
    use poise_engine::ports::OrderRequest;
    use poise_engine::runtime::{AutoState, ControlState, ExecutorState, RiskState, TrackState};
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;

    use crate::mutation_executor::test_support::{
        MemoryRepository, seeded_manager, track_write_services,
    };
    use crate::{
        ConfiguredTrackDefinition, ConfiguredTrackInput, EffectStatus, PersistedTrackEffect,
        PreparedTrackRegistry, StoredTrackEvent, StoredTrackSnapshot, TrackQueryStore,
        track_read_source_loader::TrackReadSourceLoader,
    };

    use super::TrackQueryService;

    fn test_track_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        }
    }

    #[tokio::test]
    async fn list_track_sources_reads_all_registered_snapshots() {
        let (service, repository) = test_query_service();
        let sources = service.list_track_sources().await.unwrap();

        assert!(!sources.is_empty());
        assert_eq!(sources[0].track_id, "btc-core");
        assert_eq!(
            sources[0].updated_at,
            Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap()
        );
        assert_eq!(sources[0].active_binding_count, 0);
        assert_eq!(repository.recorded_effect_limits(), Vec::<usize>::new());
    }

    #[tokio::test]
    async fn load_detail_source_reads_snapshot_events_and_effects() {
        let (service, _) = test_query_service();
        let source = service
            .load_track_detail_source(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(source.track_id, "btc-core");
        assert_eq!(
            source.updated_at,
            Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap()
        );
        assert_eq!(source.recent_activity.len(), 1);
        assert_eq!(source.recent_activity[0].message, "submit order executing");
    }

    #[tokio::test]
    async fn internal_loader_reads_raw_source_for_detail_paths() {
        let source = TrackReadSourceLoader::new(
            Arc::new(FakeReadRepository::new()),
            test_prepared_registry(),
            None,
        )
        .load_track_read_source(&TrackId::new("btc-core"))
        .await
        .unwrap()
        .unwrap();

        assert_eq!(source.recent_track_events.len(), 1);
        assert_eq!(source.recent_effects.len(), 1);
    }

    #[tokio::test]
    async fn load_track_detail_source_merges_durable_snapshot_and_live_view() {
        let repository = Arc::new(FakeReadRepository::new());
        let live_repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), live_repository);
        services
            .observation
            .observe_market(
                "btc-core",
                MarketObservation {
                    mark_price: 95.0,
                    execution_quote: Some(ExecutionQuote {
                        best_bid: 94.5,
                        best_ask: 95.5,
                    }),
                },
            )
            .await
            .unwrap();
        let service = TrackQueryService::new_with_observation(
            repository,
            test_prepared_registry(),
            Some(Arc::new(services.observation)),
        );

        let source = service
            .load_track_detail_source(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(source.mark_price, Some(95.0));
        assert_eq!(source.best_bid, Some(94.5));
        assert_eq!(source.best_ask, Some(95.5));
        assert_eq!(source.strategy_price, Some(95.0));
    }

    fn test_query_service() -> (TrackQueryService, Arc<FakeReadRepository>) {
        let repository = Arc::new(FakeReadRepository::new());
        let service = TrackQueryService::new(repository.clone(), test_prepared_registry());
        (service, repository)
    }

    fn test_prepared_registry() -> Arc<PreparedTrackRegistry> {
        Arc::new(
            PreparedTrackRegistry::new(vec![
                ConfiguredTrackDefinition::try_from_input(ConfiguredTrackInput {
                    track_id: TrackId::new("btc-core"),
                    venue: Venue::Binance,
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(ShapeFamily::Linear),
                    out_of_band_policy: Some(BandProtectionPolicy::Freeze),
                    max_notional: None,
                    daily_loss_limit: 100.0,
                    total_loss_limit: 300.0,
                    tick_timeout_secs: Some(30),
                })
                .unwrap(),
            ])
            .unwrap(),
        )
    }

    struct FakeReadRepository {
        snapshots: HashMap<String, StoredTrackSnapshot>,
        events: HashMap<String, Vec<StoredTrackEvent>>,
        effects: HashMap<String, Vec<PersistedTrackEffect>>,
        effect_limits: std::sync::Mutex<Vec<usize>>,
    }

    impl FakeReadRepository {
        fn new() -> Self {
            let snapshot = test_snapshot();
            let track_id = snapshot.track_id.clone();
            let updated_at = Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap();

            Self {
                snapshots: HashMap::from([(
                    track_id.as_str().to_string(),
                    StoredTrackSnapshot {
                        snapshot,
                        updated_at,
                    },
                )]),
                events: HashMap::from([(
                    track_id.as_str().to_string(),
                    vec![StoredTrackEvent {
                        id: 1,
                        track_id: track_id.clone(),
                        event: DomainEvent::ExposureTargetChanged {
                            from: Exposure(3.5),
                            to: Exposure(4.0),
                        },
                        created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
                    }],
                )]),
                effects: HashMap::from([(
                    track_id.as_str().to_string(),
                    vec![PersistedTrackEffect {
                        effect_id: "btc-core:batch-1:0".into(),
                        track_id,
                        batch_id: "batch-1".into(),
                        sequence: 0,
                        effect: TrackEffect::SubmitOrder {
                            request: OrderRequest {
                                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                                side: Side::Buy,
                                price: 100.5,
                                quantity: 0.1,
                                client_order_id: "client-1".into(),
                                reduce_only: false,
                            },
                            desired_exposure: Exposure(4.0),
                            submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                            recovery_token: SubmitRecoveryToken::empty(),
                        },
                        status: EffectStatus::Executing,
                        attempt_count: 1,
                        last_error: None,
                        created_at: updated_at,
                        updated_at,
                    }],
                )]),
                effect_limits: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn recorded_effect_limits(&self) -> Vec<usize> {
            self.effect_limits.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TrackQueryStore for FakeReadRepository {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            Ok(self.snapshots.values().cloned().collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            Ok(self.snapshots.get(track_id.as_str()).cloned())
        }

        async fn list_recent_track_events(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            Ok(self
                .events
                .get(track_id.as_str())
                .cloned()
                .unwrap_or_default())
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
            self.effect_limits.lock().unwrap().push(limit);
            Ok(self
                .effects
                .get(track_id.as_str())
                .cloned()
                .unwrap_or_default())
        }
    }

    fn test_snapshot() -> TrackRuntimeSnapshot {
        let config = test_track_config();
        TrackRuntimeSnapshot {
            track_id: TrackId::new("btc-core"),
            restore_revision: TrackRestoreRevision::for_track(
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                &config,
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
                unrealized_pnl: 265.2,
                ..RiskState::default()
            },
            observed: ObservedState {
                strategy_price: Some(101.25),
                strategy_price_status: poise_engine::runtime::StrategyPriceStatus::Live,
                mark_price: Some(101.5),
                best_bid: Some(101.0),
                best_ask: Some(101.5),
                out_of_band_since: None,
                last_tick_at: None,
                market_data_stale_since: None,
            },
        }
    }
}
