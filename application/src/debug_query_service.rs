use std::sync::Arc;

use anyhow::Result;
use poise_core::events::DomainEvent;
use poise_core::track::TrackId;

use crate::{
    DiagnosticSeverity, StoredTrackEvent, TrackDiagnosticItem, TrackObservationService,
    TrackQueryStore, track_diagnostic_event_loader::TrackDiagnosticEventLoader,
};

pub struct TrackDebugQueryService {
    loader: Arc<TrackDiagnosticEventLoader>,
}

impl TrackDebugQueryService {
    pub fn new(
        repository: Arc<dyn TrackQueryStore>,
        observation: Arc<TrackObservationService>,
    ) -> Self {
        Self {
            loader: Arc::new(TrackDiagnosticEventLoader::new(repository, observation)),
        }
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
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::ports::OrderRequest;

    use crate::mutation_executor::test_support::{
        MemoryRepository, seeded_manager, track_write_services,
    };
    use crate::{
        DiagnosticSeverity, EffectStatus, PersistedTrackEffect, StoredTrackEvent, TrackQueryStore,
    };

    use super::TrackDebugQueryService;

    #[tokio::test]
    async fn load_track_diagnostics_projects_only_diagnostic_events_in_order() {
        let repository = Arc::new(FakeReadRepository::new());
        let live_repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), live_repository);
        let service = TrackDebugQueryService::new(repository, Arc::new(services.observation));

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
    async fn load_track_diagnostics_does_not_require_track_definition_registry_or_effects() {
        let repository = Arc::new(FakeReadRepository::new());
        let live_repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), live_repository);
        let service =
            TrackDebugQueryService::new(repository.clone(), Arc::new(services.observation));

        let diagnostics = service
            .load_track_diagnostics(&TrackId::new("btc-core"))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(diagnostics.len(), 2);
        assert_eq!(repository.effect_query_count(), 0);
    }

    struct FakeReadRepository {
        updated_at: chrono::DateTime<Utc>,
        events: Vec<StoredTrackEvent>,
        effects: Vec<PersistedTrackEffect>,
        effect_query_count: std::sync::Mutex<usize>,
    }

    impl FakeReadRepository {
        fn new() -> Self {
            let track_id = TrackId::new("btc-core");

            Self {
                updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
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
            Ok((track_id.as_str() == "btc-core").then_some(self.updated_at))
        }
    }
}
