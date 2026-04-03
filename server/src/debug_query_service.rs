use std::sync::Arc;

use anyhow::Result;
use poise_engine::track::TrackId;
use poise_protocol::{TrackDiagnosticItemView, TrackDiagnosticsView};

use crate::event_presentation::{PresentationAudience, classify_track_events};
use crate::query_service::TrackQueryService;

pub struct TrackDebugQueryService {
    query_service: Arc<TrackQueryService>,
}

impl TrackDebugQueryService {
    pub fn new(query_service: Arc<TrackQueryService>) -> Self {
        Self { query_service }
    }

    pub async fn load_track_diagnostics(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackDiagnosticsView>> {
        let Some(source) = self.query_service.load_track_detail_source(track_id).await? else {
            return Ok(None);
        };

        let items = classify_track_events(&source)
            .into_iter()
            .filter(|item| item.audience == PresentationAudience::Diagnostics)
            .map(|item| TrackDiagnosticItemView {
                ts: item.ts.to_rfc3339(),
                message: item.message,
                level: item.level,
            })
            .collect();

        Ok(Some(TrackDiagnosticsView { items }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
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

    use super::TrackDebugQueryService;
    use crate::query_service::TrackQueryService;

    #[tokio::test]
    async fn load_track_diagnostics_projects_only_diagnostic_events_in_order() {
        let repository = Arc::new(FakeReadRepository::new());
        let service = TrackDebugQueryService::new(Arc::new(TrackQueryService::new(repository)));

        let diagnostics = service
            .load_track_diagnostics(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(diagnostics.items.len(), 2);
        assert_eq!(
            diagnostics.items[0].message,
            "target exposure 3.5000 -> 4.0000"
        );
        assert_eq!(diagnostics.items[0].ts, "2026-03-26T10:01:00+00:00");
        assert_eq!(
            diagnostics.items[1].message,
            "target exposure 4.0000 -> 4.5000"
        );
        assert_eq!(diagnostics.items[1].ts, "2026-03-26T10:01:10+00:00");
    }

    struct FakeReadRepository {
        snapshot: StoredTrackSnapshot,
        events: Vec<StoredTrackEvent>,
        effects: Vec<PersistedTrackEffect>,
    }

    impl FakeReadRepository {
        fn new() -> Self {
            let snapshot = test_snapshot();
            let track_id = snapshot.track_id.clone();

            Self {
                snapshot: StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                },
                events: vec![
                    StoredTrackEvent {
                        id: 1,
                        track_id: track_id.clone(),
                        event: DomainEvent::ExposureTargetChanged {
                            from: Exposure(4.0),
                            to: Exposure(4.5),
                        },
                        created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 10).unwrap(),
                    },
                    StoredTrackEvent {
                        id: 2,
                        track_id: track_id.clone(),
                        event: DomainEvent::ReplacementGateApplied {
                            reason: poise_core::events::ReplacementGateReason::RoundedMatch,
                        },
                        created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 5).unwrap(),
                    },
                    StoredTrackEvent {
                        id: 3,
                        track_id,
                        event: DomainEvent::ExposureTargetChanged {
                            from: Exposure(3.5),
                            to: Exposure(4.0),
                        },
                        created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
                    },
                ],
                effects: vec![PersistedTrackEffect {
                    effect_id: "btc-core:batch-1:0".into(),
                    track_id: TrackId::new("btc-core"),
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
                        target_exposure: Exposure(4.0),
                    },
                    status: EffectStatus::Failed,
                    attempt_count: 1,
                    last_error: Some("submit order rejected".into()),
                    created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                    updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                }],
            }
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for FakeReadRepository {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            Ok(vec![self.snapshot.clone()])
        }

        async fn load_track_snapshot(&self, track_id: &TrackId) -> Result<Option<StoredTrackSnapshot>> {
            Ok((track_id.as_str() == "btc-core").then_some(self.snapshot.clone()))
        }

        async fn list_recent_track_events(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            Ok((track_id.as_str() == "btc-core").then_some(self.events.clone()).unwrap_or_default())
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok((track_id.as_str() == "btc-core").then_some(self.effects.clone()).unwrap_or_default())
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
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(0.5),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 26, 10, 0, 0).unwrap()),
                last_reprice_at: None,
                slots: vec![ExecutionSlot {
                    slot: OrderSlot::new("inventory_core"),
                    state: SlotState::Working,
                    working_order: Some(WorkingOrder {
                        order_id: Some("order-1".into()),
                        client_order_id: "client-1".into(),
                        side: Side::Buy,
                        price: 100.5,
                        quantity: 0.1,
                        target_exposure: Exposure(4.0),
                        status: OrderStatus::New,
                        role: OrderRole::IncreaseInventory,
                    }),
                }],
                recent_terminal_orders: Vec::new(),
                last_execution_reason: None,
                recovery_anomaly: None,
                stats: ExecutionStats {
                    started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
                    max_inventory_gap_abs: Exposure(0.5),
                    max_gap_age_ms: 60_000,
                },
            },
            replacement_gate_reason: None,
            risk: RiskState {
                realized_pnl_day: None,
                realized_pnl_today: 0.0,
                realized_pnl_cumulative: 980.1,
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
