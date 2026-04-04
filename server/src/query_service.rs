use std::sync::Arc;

use anyhow::Result;
use poise_engine::ports::TrackReadRepositoryPort;
use poise_engine::track::TrackId;

use crate::read_model::TrackReadModel;

const LIST_EFFECTS_LIMIT: usize = 20;
const DETAIL_EVENTS_LIMIT: usize = 20;
const DETAIL_EFFECTS_LIMIT: usize = 20;

pub struct TrackQueryService {
    repository: Arc<dyn TrackReadRepositoryPort>,
}

impl TrackQueryService {
    pub fn new(repository: Arc<dyn TrackReadRepositoryPort>) -> Self {
        Self { repository }
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

            sources.push(TrackReadModel::from_snapshot(
                snapshot.snapshot,
                snapshot.updated_at,
                Vec::new(),
                recent_effects,
            ));
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

        Ok(Some(TrackReadModel::from_snapshot(
            snapshot.snapshot,
            snapshot.updated_at,
            recent_track_events,
            recent_effects,
        )))
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
    use poise_engine::ports::{
        EffectStatus, OrderRequest, OrderStatus, PersistedTrackEffect, StoredTrackEvent,
        StoredTrackSnapshot, TrackReadRepositoryPort,
    };
    use poise_engine::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, RiskState, SlotState, TrackStatus,
        WorkingOrder,
    };
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;

    use crate::projector::TrackProjector;
    use crate::read_model::TrackReadModel;

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

    #[test]
    fn read_model_from_snapshot_flattens_runtime_state() {
        let repository = FakeReadRepository::new();
        let stored = repository
            .snapshots
            .get("btc-core")
            .expect("seeded snapshot should exist");
        let event = repository.events.get("btc-core").unwrap()[0].clone();
        let effect = repository.effects.get("btc-core").unwrap()[0].clone();

        let read_model = TrackReadModel::from_snapshot(
            stored.snapshot.clone(),
            stored.updated_at,
            vec![event],
            vec![effect],
        );

        assert_eq!(read_model.track_id, "btc-core");
        assert_eq!(read_model.venue, "binance");
        assert_eq!(read_model.symbol, "BTCUSDT");
        assert_eq!(read_model.reference_price, Some(101.25));
        assert_eq!(read_model.current_exposure, 3.5);
        assert_eq!(read_model.desired_exposure, Some(4.0));
        assert_eq!(read_model.executor_mode, ExecutionMode::Passive);
        assert_eq!(read_model.inventory_gap, 0.5);
        assert_eq!(read_model.max_inventory_gap_abs, 0.5);
        assert_eq!(read_model.slots.len(), 1);
        assert_eq!(read_model.slots[0].label, "inventory");
        assert!(!read_model.slots[0].is_submit_pending);
        assert_eq!(read_model.recent_track_events.len(), 1);
        assert_eq!(read_model.recent_effects.len(), 1);
    }

    #[test]
    fn query_service_projects_desired_exposure_as_target_exposure_for_clients() {
        let read_model = TrackReadModel::from_snapshot(
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
                replacement_gate_reason: None,
                risk: RiskState {
                    realized_pnl_day: None,
                    realized_pnl_today: 0.0,
                    realized_pnl_cumulative: 0.0,
                    unrealized_pnl: 0.0,
                    ..RiskState::default()
                },
                observed: ObservedState {
                    reference_price: Some(101.25),
                    out_of_band_since: None,
                    last_tick_at: None,
                    market_data_stale_since: None,
                },
            },
            Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(read_model.desired_exposure, Some(4.0));
        let projected = TrackProjector::new().project_detail(&read_model);
        assert_eq!(projected.position.target_exposure, Some(4.0));
    }

    fn test_query_service() -> (TrackQueryService, Arc<FakeReadRepository>) {
        let repository = Arc::new(FakeReadRepository::new());
        let service = TrackQueryService::new(repository.clone());
        (service, repository)
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
            let track_id = snapshot.track_id.as_str().to_string();
            let snapshot_updated_at = Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap();

            let event = StoredTrackEvent {
                id: 1,
                track_id: snapshot.track_id.clone(),
                event: DomainEvent::BandBreached {
                    boundary: poise_core::strategy::BandBoundary::Above,
                    price: 120.0,
                },
                created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
            };

            let effect = PersistedTrackEffect {
                effect_id: "btc-core:batch-1:0".into(),
                track_id: snapshot.track_id.clone(),
                batch_id: "batch-1".into(),
                sequence: 0,
                effect: TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: snapshot.instrument.clone(),
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
                created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            };

            Self {
                snapshots: HashMap::from([(
                    track_id.clone(),
                    StoredTrackSnapshot {
                        snapshot,
                        updated_at: snapshot_updated_at,
                    },
                )]),
                events: HashMap::from([(track_id.clone(), vec![event])]),
                effects: HashMap::from([(track_id, vec![effect])]),
                effect_limits: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn recorded_effect_limits(&self) -> Vec<usize> {
            self.effect_limits.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TrackReadRepositoryPort for FakeReadRepository {
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
                .unwrap_or_default()
                .into_iter()
                .take(limit)
                .collect())
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
            replacement_gate_reason: None,
            risk: RiskState {
                realized_pnl_day: None,
                realized_pnl_today: 0.0,
                realized_pnl_cumulative: 0.0,
                unrealized_pnl: 0.0,
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
