use std::sync::Arc;

use anyhow::Result;
use poise_engine::track::TrackId;

use crate::{
    PreparedTrackRegistry, TrackQueryStore, TrackReadModel, TrackReadSource, TrackRuntimeReadState,
};

const LIST_EFFECTS_LIMIT: usize = 20;
const DETAIL_EVENTS_LIMIT: usize = 20;
const DETAIL_EFFECTS_LIMIT: usize = 20;

pub struct TrackQueryService {
    repository: Arc<dyn TrackQueryStore>,
    prepared_registry: Arc<PreparedTrackRegistry>,
}

impl TrackQueryService {
    pub fn new(
        repository: Arc<dyn TrackQueryStore>,
        prepared_registry: Arc<PreparedTrackRegistry>,
    ) -> Self {
        Self {
            repository,
            prepared_registry,
        }
    }

    pub async fn list_track_sources(&self) -> Result<Vec<TrackReadModel>> {
        let mut snapshots = self.repository.list_track_snapshots().await?;
        snapshots.sort_by(|left, right| {
            left.snapshot
                .track_id
                .as_str()
                .cmp(right.snapshot.track_id.as_str())
        });

        let mut sources = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let recent_effects = self
                .repository
                .list_recent_track_effects(&snapshot.snapshot.track_id, LIST_EFFECTS_LIMIT)
                .await?;
            let source = self.read_source_from_snapshot(
                snapshot.snapshot.track_id.clone(),
                snapshot.snapshot,
                snapshot.updated_at,
                Vec::new(),
                recent_effects,
            )?;

            sources.push(TrackReadModel::from_source(source));
        }

        Ok(sources)
    }

    pub async fn load_track_detail_source(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackReadModel>> {
        let Some(snapshot) = self.repository.load_track_snapshot(track_id).await? else {
            return Ok(None);
        };

        let recent_track_events = self
            .repository
            .list_recent_track_events(track_id, DETAIL_EVENTS_LIMIT)
            .await?;
        let recent_effects = self
            .repository
            .list_recent_track_effects(track_id, DETAIL_EFFECTS_LIMIT)
            .await?;
        Ok(Some(TrackReadModel::from_source(
            self.read_source_from_snapshot(
                track_id.clone(),
                snapshot.snapshot,
                snapshot.updated_at,
                recent_track_events,
                recent_effects,
            )?,
        )))
    }

    fn read_source_from_snapshot(
        &self,
        track_id: TrackId,
        snapshot: poise_engine::snapshot::TrackRuntimeSnapshot,
        updated_at: chrono::DateTime<chrono::Utc>,
        recent_track_events: Vec<crate::StoredTrackEvent>,
        recent_effects: Vec<crate::PersistedTrackEffect>,
    ) -> Result<TrackReadSource> {
        let definition = self
            .prepared_registry
            .get(&track_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "missing prepared track definition for `{}`",
                    track_id.as_str()
                )
            })?
            .read_definition();

        Ok(TrackReadSource {
            definition,
            runtime: TrackRuntimeReadState::from_snapshot(snapshot),
            updated_at,
            recent_track_events,
            recent_effects,
        })
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
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{Exposure, Side};
    use poise_engine::executor::{ExecutionMode, OrderRole, OrderSlot};
    use poise_engine::ports::{OrderRequest, OrderStatus};
    use poise_engine::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, RiskState, SlotState, TrackStatus,
        WorkingOrder,
    };
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;

    use crate::{
        ConfiguredTrackDefinition, ConfiguredTrackInput, EffectStatus, PersistedTrackEffect,
        PreparedTrackRegistry, StoredTrackEvent, StoredTrackSnapshot, TrackQueryStore,
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
        assert_eq!(sources[0].recent_track_events.len(), 0);
        assert_eq!(sources[0].recent_effects.len(), 1);
        assert_eq!(sources[0].recent_effects[0].status, EffectStatus::Executing);
        assert_eq!(
            repository.recorded_effect_limits(),
            vec![super::LIST_EFFECTS_LIMIT]
        );
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
        assert_eq!(source.recent_track_events.len(), 1);
        assert_eq!(source.recent_effects.len(), 1);
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
                    out_of_band_policy: Some(OutOfBandPolicy::Freeze),
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
        TrackRuntimeSnapshot {
            track_id: TrackId::new("btc-core"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            config: TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            status: TrackStatus::Active,
            current_exposure: Exposure(3.5),
            desired_exposure: Some(Exposure(4.0)),
            manual_target_override: None,
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
            risk: RiskState {
                unrealized_pnl: 265.2,
                ..RiskState::default()
            },
            observed: ObservedState {
                reference_price: Some(101.25),
                out_of_band_since: None,
                last_tick_at: None,
                market_data_stale_since: None,
            },
        }
    }
}
