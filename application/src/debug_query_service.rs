use std::sync::Arc;

use anyhow::Result;
use poise_core::events::DomainEvent;
use poise_engine::track::TrackId;

use crate::{
    DiagnosticSeverity, StoredTrackEvent, TrackDiagnosticItem,
    track_diagnostic_event_loader::TrackDiagnosticEventLoader,
};

pub struct TrackDebugQueryService {
    loader: Arc<TrackDiagnosticEventLoader>,
}

impl TrackDebugQueryService {
    pub(crate) fn from_loader(loader: Arc<TrackDiagnosticEventLoader>) -> Self {
        Self { loader }
    }

    pub async fn load_track_diagnostics(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<Vec<TrackDiagnosticItem>>> {
        let Some(events) = self.loader.load_recent_track_events(track_id).await? else {
            return Ok(None);
        };

        Ok(Some(classify_diagnostic_items(&events)))
    }
}

fn classify_diagnostic_items(events: &[StoredTrackEvent]) -> Vec<TrackDiagnosticItem> {
    let mut items = events
        .iter()
        .filter_map(|event| match &event.event {
            DomainEvent::ExposureTargetChanged { from, to } => Some(TrackDiagnosticItem {
                observed_at: event.created_at,
                severity: DiagnosticSeverity::Info,
                message: format!("desired exposure {:.4} -> {:.4}", from.0, to.0),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    items.sort_by_key(|item| item.observed_at);
    items
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use poise_core::events::DomainEvent;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{Exposure, Side};
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::persisted_runtime::TrackRestoreRevision;
    use poise_engine::ports::OrderRequest;
    use poise_engine::runtime::{AutoState, ControlState, ExecutorState, RiskState, TrackState};
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;

    use crate::{
        ConfiguredTrackDefinition, ConfiguredTrackInput, DiagnosticSeverity, EffectStatus,
        PersistedTrackEffect, PreparedTrackRegistry, StoredTrackEvent, StoredTrackSnapshot,
        TrackQueryStore, TrackReadServices,
    };

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
    async fn load_track_diagnostics_projects_only_diagnostic_events_in_order() {
        let repository = Arc::new(FakeReadRepository::new());
        let read_services = TrackReadServices::new(repository, test_prepared_registry());
        let service = read_services.debug_query_service();

        let diagnostics = service
            .load_track_diagnostics(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Info);
        assert_eq!(diagnostics[0].message, "desired exposure 3.5000 -> 4.0000");
        assert_eq!(
            diagnostics[0].observed_at,
            Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap()
        );
        assert_eq!(diagnostics[1].message, "desired exposure 4.0000 -> 4.5000");
        assert_eq!(
            diagnostics[1].observed_at,
            Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 10).unwrap()
        );
    }

    #[tokio::test]
    async fn load_track_diagnostics_does_not_require_prepared_registry_or_effects() {
        let repository = Arc::new(FakeReadRepository::new());
        let read_services = TrackReadServices::new(repository.clone(), Arc::default());
        let service = read_services.debug_query_service();

        let diagnostics = service
            .load_track_diagnostics(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(diagnostics.len(), 2);
        assert_eq!(repository.effect_query_count(), 0);
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
        snapshot: StoredTrackSnapshot,
        events: Vec<StoredTrackEvent>,
        effects: Vec<PersistedTrackEffect>,
        effect_query_count: std::sync::Mutex<usize>,
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
                        desired_exposure: Exposure(4.0),
                        submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                        recovery_token: SubmitRecoveryToken::empty(),
                    },
                    status: EffectStatus::Failed,
                    attempt_count: 1,
                    last_error: Some("submit order rejected".into()),
                    created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                    updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                }],
                effect_query_count: std::sync::Mutex::new(0),
            }
        }

        fn effect_query_count(&self) -> usize {
            *self.effect_query_count.lock().unwrap()
        }
    }

    #[async_trait]
    impl TrackQueryStore for FakeReadRepository {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            Ok(vec![self.snapshot.clone()])
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            Ok((track_id.as_str() == "btc-core").then_some(self.snapshot.clone()))
        }

        async fn list_recent_track_events(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            Ok(if track_id.as_str() == "btc-core" {
                self.events.clone()
            } else {
                Vec::new()
            })
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
            *self.effect_query_count.lock().unwrap() += 1;
            Ok(if track_id.as_str() == "btc-core" {
                self.effects.clone()
            } else {
                Vec::new()
            })
        }

        async fn load_track_persistent_state(
            &self,
            _track_id: &TrackId,
        ) -> Result<Option<crate::TrackPersistentState>> {
            Ok(None)
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
            execution_gate_state: poise_engine::execution_gate::ExecutionGateState::open(),
            ledger_state: Default::default(),
            risk: RiskState {
                unrealized_pnl: 0.0,
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
