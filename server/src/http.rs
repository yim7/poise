use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use poise_engine::command::TrackCommand;
use poise_engine::track::TrackId;
use poise_protocol::{
    TrackCommandAccepted, TrackCommandRequest, TrackCommandType, TrackDetailView,
    TrackDiagnosticsView, TrackListResponse,
};
use serde::Serialize;
use tower_http::cors::CorsLayer;

use crate::assembly::ServerState;
use crate::write_service::TrackMutationError;

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct HealthResponse {
    status: String,
    track_count: usize,
    attention_required_count: usize,
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tracks", get(list_tracks))
        .route("/tracks/:id", get(get_track_detail))
        .route("/debug/tracks/:id/diagnostics", get(get_track_diagnostics))
        .route("/tracks/:id/commands", post(submit_command))
        .route("/ws", get(crate::websocket::ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn list_tracks(
    State(state): State<ServerState>,
) -> Result<Json<TrackListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sources = state
        .query_service
        .list_track_sources()
        .await
        .map_err(map_query_error)?;
    let items = sources
        .iter()
        .map(|source| state.projector.project_list_item(source))
        .collect();
    Ok(Json(TrackListResponse { items }))
}

async fn health(
    State(state): State<ServerState>,
) -> Result<(StatusCode, Json<HealthResponse>), (StatusCode, Json<ErrorResponse>)> {
    let sources = state
        .query_service
        .list_track_sources()
        .await
        .map_err(map_query_error)?;
    let attention_required_count = sources
        .iter()
        .filter(|source| {
            source.has_recovery_anomaly
                || source.has_account_margin_guard
                || source.has_stale_market_data
        })
        .count();
    let status = if attention_required_count == 0 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    Ok((
        status,
        Json(HealthResponse {
            status: if attention_required_count == 0 {
                "ok".to_string()
            } else {
                "attention_required".to_string()
            },
            track_count: sources.len(),
            attention_required_count,
        }),
    ))
}

async fn get_track_detail(
    Path(id): Path<String>,
    State(state): State<ServerState>,
) -> Result<Json<TrackDetailView>, (StatusCode, Json<ErrorResponse>)> {
    let track_id = TrackId::new(id.clone());
    let source = state
        .query_service
        .load_track_detail_source(&track_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;
    Ok(Json(state.projector.project_detail(&source)))
}

async fn get_track_diagnostics(
    Path(id): Path<String>,
    State(state): State<ServerState>,
) -> Result<Json<TrackDiagnosticsView>, (StatusCode, Json<ErrorResponse>)> {
    let track_id = TrackId::new(id.clone());
    let diagnostics = state
        .debug_query_service
        .load_track_diagnostics(&track_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;

    Ok(Json(diagnostics))
}

async fn submit_command(
    Path(id): Path<String>,
    State(state): State<ServerState>,
    Json(request): Json<TrackCommandRequest>,
) -> Result<Json<TrackCommandAccepted>, (StatusCode, Json<ErrorResponse>)> {
    if !state.write_service.has_track(&id).await {
        return Err(not_found(format!("track `{id}` not found")));
    }

    let command = map_command(request.command)?;
    state
        .write_service
        .command(&id, command)
        .await
        .map_err(map_command_error)?;

    Ok(Json(TrackCommandAccepted {
        track_id: id,
        command: request.command,
        accepted: true,
    }))
}

fn map_command(
    command: TrackCommandType,
) -> Result<TrackCommand, (StatusCode, Json<ErrorResponse>)> {
    match command {
        TrackCommandType::Pause => Ok(TrackCommand::Pause),
        TrackCommandType::Resume => Ok(TrackCommand::Resume),
        TrackCommandType::Terminate => Ok(TrackCommand::Terminate),
        TrackCommandType::Flatten => Ok(TrackCommand::Flatten),
    }
}

fn bad_request(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse { error: message }),
    )
}

fn not_found(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse { error: message }),
    )
}

fn internal_error(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse { error: message }),
    )
}

fn map_query_error(error: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    internal_error(error.to_string())
}

fn map_command_error(error: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    match error.downcast::<TrackMutationError>() {
        Ok(TrackMutationError::LoadedTrackInvariant { track_id }) => {
            internal_error(TrackMutationError::LoadedTrackInvariant { track_id }.to_string())
        }
        Ok(TrackMutationError::Mutation(error)) => bad_request(error.to_string()),
        Ok(TrackMutationError::Persistence(error)) => internal_error(error.to_string()),
        Err(error) => internal_error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::anyhow;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use chrono::Utc;
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::{
        events::DomainEvent,
        types::{ExchangeRules, Exposure},
    };
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        ClockPort, OrderStatus, StateRepositoryPort, StoredTrackSnapshot, TrackReadRepositoryPort,
    };
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_protocol::{
        ExecutionIntentView, ExecutionSlotPhaseView, ExecutionStatusView, TrackCommandAccepted,
        TrackCommandRequest, TrackCommandType, TrackDetailView, TrackDiagnosticsView,
        TrackListResponse, TrackStatus,
    };
    use poise_storage::sqlite::SqliteStorage;
    use tower::ServiceExt;

    use crate::assembly::{ServerState, build_server_state};
    use crate::notifications::TrackInternalNotification;
    use crate::projector::TrackProjector;
    use crate::query_service::TrackQueryService;
    use crate::write_service::TrackWriteService;

    use super::router;

    fn test_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.0,
            quantity_step: 0.0,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }

    async fn app_state() -> ServerState {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        build_test_state(repository).await
    }

    async fn build_test_state<R>(repository: Arc<R>) -> ServerState
    where
        R: StateRepositoryPort + TrackReadRepositoryPort + 'static,
    {
        let manager = test_manager();
        let mut snapshot = manager
            .snapshot("btc-core")
            .expect("seeded manager should expose runtime snapshot");
        snapshot.risk.realized_pnl_cumulative = 980.1;
        snapshot.risk.unrealized_pnl = 265.2;
        repository
            .save_transition(
                "btc-core",
                &snapshot,
                &[DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                }],
                &[],
            )
            .await
            .unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel::<TrackInternalNotification>(16);
        let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
        let read_repository: Arc<dyn TrackReadRepositoryPort> = repository;
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            state_repository.clone(),
            notifications,
        ));

        let query_service = Arc::new(TrackQueryService::new(read_repository));
        build_server_state(
            write_service,
            state_repository,
            query_service,
            Arc::new(TrackProjector::new()),
        )
    }

    fn test_manager() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(FakeClock));
        manager
            .add_track(
                TrackId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                TrackConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: 0.5,
                    shape_family: ShapeFamily::Linear,
                    out_of_band_policy: OutOfBandPolicy::Freeze,
                },
                CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: -100.0,
                    stop_loss_pct: 10.0,
                },
                test_exchange_rules(),
            )
            .unwrap();
        manager
            .observe(
                &TrackId::new("btc-core"),
                poise_engine::observation::TrackObservation::Market(
                    poise_engine::observation::MarketObservation {
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();
        let track = manager
            .get_track("btc-core")
            .expect("track should still exist")
            .clone();
        let mut snapshot = track.snapshot();
        let slot_order = snapshot
            .executor_state
            .slots
            .first_mut()
            .and_then(|slot| slot.working_order.as_mut())
            .expect("market observe should seed inventory_core working order");
        slot_order.order_id = Some("order-1".into());
        slot_order.status = OrderStatus::New;
        manager.restore_track_state(&snapshot).unwrap();
        manager
    }

    #[tokio::test]
    async fn list_tracks_returns_track_list_response() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/tracks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackListResponse = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.items.len(), 1);
        assert_eq!(payload.items[0].id, "btc-core");
        assert_eq!(payload.items[0].instrument.symbol, "BTCUSDT");
        assert_eq!(
            payload.items[0].execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert_eq!(payload.items[0].execution.active_slot_count, 1);
    }

    #[tokio::test]
    async fn health_returns_ok_for_normal_runtime_state() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["track_count"], 1);
        assert_eq!(payload["attention_required_count"], 0);
    }

    #[tokio::test]
    async fn health_returns_service_unavailable_when_attention_required_present() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let state = build_test_state(repository.clone()).await;
        let mut snapshot = test_manager()
            .snapshot("btc-core")
            .expect("seeded manager should expose runtime snapshot");
        snapshot.observed.market_data_stale_since = Some(Utc::now());
        repository
            .save_transition("btc-core", &snapshot, &[], &[])
            .await
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["status"], "attention_required");
        assert_eq!(payload["track_count"], 1);
        assert_eq!(payload["attention_required_count"], 1);
    }

    #[tokio::test]
    async fn get_track_detail_returns_track_detail_view() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDetailView = serde_json::from_slice(&body).unwrap();
        let payload_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.identity.id, "btc-core");
        assert_eq!(payload.identity.instrument.symbol, "BTCUSDT");
        assert_eq!(
            payload_json["strategy"]["long_exposure_units"].as_f64(),
            Some(8.0)
        );
        assert_eq!(
            payload_json["strategy"]["short_exposure_units"].as_f64(),
            Some(8.0)
        );
        assert_eq!(
            payload_json["strategy"]["notional_per_unit"].as_f64(),
            Some(375.0)
        );
        assert_eq!(
            payload_json["strategy"]["min_rebalance_units"].as_f64(),
            Some(0.5)
        );
        assert!((payload.pnl.realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((payload.pnl.total_pnl - 1245.3).abs() < f64::EPSILON);
        assert!((payload.pnl.unrealized_pnl - 265.2).abs() < f64::EPSILON);
        assert_eq!(
            payload_json["execution_stats"]["max_inventory_gap_abs"].as_f64(),
            Some(payload.execution_stats.max_inventory_gap_abs)
        );
        assert_eq!(
            payload_json["execution_stats"]["max_gap_age_ms"].as_i64(),
            Some(0)
        );
        assert!(payload.execution_stats.stats_started_at.is_some());
        assert_eq!(
            payload_json["execution_stats"]["stats_started_at"].as_str(),
            payload.execution_stats.stats_started_at.as_deref()
        );
        assert!(payload_json.get("statistics").is_none());
        assert_eq!(
            payload.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert_eq!(payload.execution.active_slot_count, 1);
        assert_eq!(payload.execution.slots.len(), 1);
        assert_eq!(payload.execution.slots[0].label, "inventory");
        assert_eq!(
            payload.execution.slots[0].phase,
            ExecutionSlotPhaseView::Opening
        );
        assert_eq!(
            payload.execution.slots[0].intent,
            ExecutionIntentView::IncreaseInventory
        );
        assert!(!payload.available_commands.is_empty());
        assert!(payload_json["execution"]["slots"][0].get("state").is_none());
        assert!(
            !payload
                .activity
                .iter()
                .any(|item| item.message.contains("client-1"))
        );
    }

    #[tokio::test]
    async fn submit_command_accepts_typed_command() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackCommandAccepted = serde_json::from_slice(&body).unwrap();

        assert!(payload.accepted);
        assert_eq!(payload.track_id, "btc-core");
        assert_eq!(payload.command, TrackCommandType::Pause);
    }

    #[tokio::test]
    async fn submit_command_accepts_flatten() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Flatten,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackCommandAccepted = serde_json::from_slice(&body).unwrap();
        assert!(payload.accepted);
        assert_eq!(payload.track_id, "btc-core");
        assert_eq!(payload.command, TrackCommandType::Flatten);
    }

    #[tokio::test]
    async fn submit_command_accepts_terminate() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Terminate,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackCommandAccepted = serde_json::from_slice(&body).unwrap();
        assert!(payload.accepted);
        assert_eq!(payload.track_id, "btc-core");
        assert_eq!(payload.command, TrackCommandType::Terminate);
    }

    #[tokio::test]
    async fn resume_command_rejects_non_paused_track() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Resume,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_command_rolls_back_detail_when_persistence_fails() {
        let repository = Arc::new(FailingRepository::default());
        let manager = test_manager();
        repository.seed_snapshot(
            manager
                .snapshot("btc-core")
                .expect("seeded manager should expose runtime snapshot"),
        );
        let (notifications, _) = tokio::sync::broadcast::channel::<TrackInternalNotification>(16);
        let state_repository = repository.clone() as Arc<dyn StateRepositoryPort>;
        let query_service = Arc::new(TrackQueryService::new(
            repository.clone() as Arc<dyn TrackReadRepositoryPort>
        ));
        let app = router(build_server_state(
            Arc::new(TrackWriteService::new(
                manager,
                state_repository.clone(),
                notifications,
            )),
            state_repository,
            query_service,
            Arc::new(TrackProjector::new()),
        ));

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let detail = app
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status.lifecycle.status, TrackStatus::Active);
    }

    #[tokio::test]
    async fn pause_command_updates_detail_status() {
        let app = router(app_state().await);

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause.status(), StatusCode::OK);

        let detail = app
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["status"]["lifecycle"]["status"], "paused");
        assert_eq!(
            payload["position"]["desired_exposure"],
            serde_json::Value::Null
        );
        assert_eq!(payload["position"].as_object().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn resume_command_reactivates_paused_track() {
        let app = router(app_state().await);

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause.status(), StatusCode::OK);

        let resume = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&TrackCommandRequest {
                            command: TrackCommandType::Resume,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resume.status(), StatusCode::OK);

        let detail = app
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status.lifecycle.status, TrackStatus::Active);
    }

    #[tokio::test]
    async fn get_track_detail_returns_404_for_missing_track() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/tracks/ETHUSDT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_track_diagnostics_returns_exposure_target_changed_events() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/debug/tracks/btc-core/diagnostics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackDiagnosticsView = serde_json::from_slice(&body).unwrap();

        assert!(
            payload
                .items
                .iter()
                .any(|item| item.message.contains("desired exposure"))
        );
    }

    #[derive(Default)]
    struct FailingRepository {
        snapshots: std::sync::Mutex<std::collections::HashMap<String, StoredTrackSnapshot>>,
    }

    impl FailingRepository {
        fn seed_snapshot(&self, snapshot: poise_engine::ports::TrackSnapshot) {
            self.snapshots.lock().unwrap().insert(
                snapshot.track_id.as_str().to_string(),
                StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                },
            );
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailingRepository {
        async fn save_transition_with_effect_status(
            &self,
            _id: &str,
            _state: &poise_engine::ports::TrackSnapshot,
            _events: &[poise_core::events::DomainEvent],
            _effects: &[poise_engine::transition::TrackEffect],
            _effect_status_update: Option<&poise_engine::ports::EffectStatusUpdate>,
        ) -> anyhow::Result<poise_engine::ports::CommittedTrackWrite> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn load_track_state(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<poise_engine::ports::TrackSnapshot>> {
            Ok(None)
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_dispatchable_effects(
            &self,
        ) -> anyhow::Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_all_pending_submit_effects(
            &self,
        ) -> anyhow::Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_track_batch(
            &self,
            _track_id: &TrackId,
            _batch_id: &str,
        ) -> anyhow::Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn save_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &poise_engine::ports::FollowUpRetirementRequest,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Vec<poise_engine::ports::FollowUpRetirementRequest>> {
            Ok(Vec::new())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &poise_engine::ports::FollowUpRetirementRequest,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for FailingRepository {
        async fn list_track_snapshots(&self) -> anyhow::Result<Vec<StoredTrackSnapshot>> {
            Ok(self.snapshots.lock().unwrap().values().cloned().collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> anyhow::Result<Option<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned())
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<poise_engine::ports::StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }
    }
}
