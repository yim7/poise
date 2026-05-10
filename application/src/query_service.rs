use std::sync::Arc;

use anyhow::Result;
use poise_core::track::TrackId;

use crate::{
    TrackDefinitionRegistry, TrackListReadModel, TrackObservationService, TrackQueryStore,
    TrackReadModel, track_read_source_loader::TrackReadSourceLoader,
};

pub struct TrackQueryService {
    loader: Arc<TrackReadSourceLoader>,
}

impl TrackQueryService {
    pub fn new(
        repository: Arc<dyn TrackQueryStore>,
        track_definition_registry: Arc<TrackDefinitionRegistry>,
        observation: Arc<TrackObservationService>,
    ) -> Self {
        Self {
            loader: Arc::new(TrackReadSourceLoader::new(
                repository,
                track_definition_registry,
                observation,
            )),
        }
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
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::observation::MarketObservation;
    use poise_engine::ports::ExecutionQuote;
    use poise_engine::ports::OrderRequest;

    use crate::mutation_executor::test_support::{
        MemoryRepository, seeded_manager, track_write_services,
    };
    use crate::{
        EffectStatus, PersistedTrackEffect, StoredTrackEvent, TrackDefinitionRegistry,
        TrackQueryStore, track_read_source_loader::TrackReadSourceLoader,
    };

    use super::TrackQueryService;

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
        let live_repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), live_repository);
        let source = TrackReadSourceLoader::new(
            Arc::new(FakeReadRepository::new()),
            test_track_definition_registry(),
            Arc::new(services.observation),
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
                MarketObservation::MarkPrice { mark_price: 95.0 },
            )
            .await
            .unwrap();
        services
            .observation
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.5,
                        best_ask: 95.5,
                    },
                },
            )
            .await
            .unwrap();
        let service = TrackQueryService::new(
            repository,
            test_track_definition_registry(),
            Arc::new(services.observation),
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

    #[tokio::test]
    async fn list_track_sources_reads_live_runtime_without_persisted_snapshot() {
        let repository = Arc::new(FakeReadRepository::without_snapshots());
        let live_repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), live_repository);
        let service = TrackQueryService::new(
            repository,
            test_track_definition_registry(),
            Arc::new(services.observation),
        );

        let sources = service.list_track_sources().await.unwrap();

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].track_id, "btc-core");
        assert_eq!(sources[0].current_exposure, 0.0);
    }

    #[tokio::test]
    async fn load_detail_source_reads_live_runtime_without_persisted_snapshot() {
        let repository = Arc::new(FakeReadRepository::without_snapshots());
        let live_repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), live_repository);
        let service = TrackQueryService::new(
            repository,
            test_track_definition_registry(),
            Arc::new(services.observation),
        );

        let source = service
            .load_track_detail_source(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(source.track_id, "btc-core");
        assert_eq!(source.current_exposure, 0.0);
        assert_eq!(source.recent_activity.len(), 1);
    }

    fn test_query_service() -> (TrackQueryService, Arc<FakeReadRepository>) {
        let repository = Arc::new(FakeReadRepository::new());
        let live_repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), live_repository);
        let service = TrackQueryService::new(
            repository.clone(),
            test_track_definition_registry(),
            Arc::new(services.observation),
        );
        (service, repository)
    }

    fn test_track_definition_registry() -> Arc<TrackDefinitionRegistry> {
        Arc::new(
            TrackDefinitionRegistry::new(vec![
                TrackDefinition::try_new(
                    TrackId::new("btc-core"),
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    poise_core::strategy::TrackConfig {
                        lower_price: 90.0,
                        upper_price: 110.0,
                        long_exposure_units: 8.0,
                        short_exposure_units: 8.0,
                        notional_per_unit: 375.0,
                        min_rebalance_units: 0.5,
                        shape_family: ShapeFamily::Linear,
                        out_of_band_policy: BandProtectionPolicy::Freeze,
                        risk_acquisition: Default::default(),
                    },
                    None,
                    LossLimits {
                        daily_loss_limit: 100.0,
                        total_loss_limit: 300.0,
                    },
                    Some(30),
                )
                .unwrap(),
            ])
            .unwrap(),
        )
    }

    struct FakeReadRepository {
        updated_at: HashMap<String, chrono::DateTime<Utc>>,
        events: HashMap<String, Vec<StoredTrackEvent>>,
        effects: HashMap<String, Vec<PersistedTrackEffect>>,
        effect_limits: std::sync::Mutex<Vec<usize>>,
    }

    impl FakeReadRepository {
        fn new() -> Self {
            let track_id = TrackId::new("btc-core");
            let updated_at = Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap();

            Self {
                updated_at: HashMap::from([(track_id.as_str().to_string(), updated_at)]),
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

        fn without_snapshots() -> Self {
            let mut repository = Self::new();
            repository.updated_at.clear();
            repository
        }

        fn recorded_effect_limits(&self) -> Vec<usize> {
            self.effect_limits.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TrackQueryStore for FakeReadRepository {
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

        async fn load_track_control_state(
            &self,
            _track_id: &TrackId,
        ) -> Result<Option<crate::TrackControlState>> {
            Ok(None)
        }

        async fn load_track_pnl_stats(
            &self,
            _track_id: &TrackId,
            pnl_utc_day: chrono::NaiveDate,
        ) -> Result<poise_engine::ledger::TrackPnlStats> {
            Ok(poise_engine::ledger::TrackPnlStats {
                pnl_utc_day,
                ..poise_engine::ledger::TrackPnlStats::default()
            })
        }

        async fn load_track_updated_at(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<chrono::DateTime<Utc>>> {
            Ok(self.updated_at.get(track_id.as_str()).copied())
        }
    }
}
