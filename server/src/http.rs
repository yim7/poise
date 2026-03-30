use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use poise_engine::command::TrackCommand;
use poise_engine::track::TrackId;
use poise_protocol::{
    GridCommandAccepted, GridCommandRequest, GridCommandType, GridDetailView, GridListResponse,
};
use serde::Serialize;
use tower_http::cors::CorsLayer;

use crate::assembly::ServerState;
use crate::write_service::TrackMutationError;

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ErrorResponse {
    error: String,
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/grids", get(list_tracks))
        .route("/grids/:id", get(get_grid_detail))
        .route("/grids/:id/commands", post(submit_command))
        .route("/ws", get(crate::websocket::ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn list_tracks(
    State(state): State<ServerState>,
) -> Result<Json<GridListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sources = state
        .query_service
        .list_track_sources()
        .await
        .map_err(map_query_error)?;
    let items = sources
        .iter()
        .map(|source| state.projector.project_list_item(source))
        .collect();
    Ok(Json(GridListResponse { items }))
}

async fn get_grid_detail(
    Path(id): Path<String>,
    State(state): State<ServerState>,
) -> Result<Json<GridDetailView>, (StatusCode, Json<ErrorResponse>)> {
    let grid_id = TrackId::new(id.clone());
    let source = state
        .query_service
        .load_track_detail_source(&grid_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("grid `{id}` not found")))?;
    Ok(Json(state.projector.project_detail(&source)))
}

async fn submit_command(
    Path(id): Path<String>,
    State(state): State<ServerState>,
    Json(request): Json<GridCommandRequest>,
) -> Result<Json<GridCommandAccepted>, (StatusCode, Json<ErrorResponse>)> {
    if !state.write_service.has_track(&id).await {
        return Err(not_found(format!("grid `{id}` not found")));
    }

    let command = map_command(request.command)?;
    state
        .write_service
        .command(&id, command)
        .await
        .map_err(map_command_error)?;

    Ok(Json(GridCommandAccepted {
        grid_id: id,
        command: request.command,
        accepted: true,
    }))
}

fn map_command(command: GridCommandType) -> Result<TrackCommand, (StatusCode, Json<ErrorResponse>)> {
    match command {
        GridCommandType::Pause => Ok(TrackCommand::Pause),
        GridCommandType::Resume => Ok(TrackCommand::Resume),
        GridCommandType::Flatten => Ok(TrackCommand::Flatten),
        GridCommandType::Terminate => Err(bad_request(format!(
            "command `{}` is not implemented",
            command_name(command)
        ))),
    }
}

fn command_name(command: GridCommandType) -> &'static str {
    match command {
        GridCommandType::Pause => "pause",
        GridCommandType::Resume => "resume",
        GridCommandType::Terminate => "terminate",
        GridCommandType::Flatten => "flatten",
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
    use poise_core::strategy::{TrackConfig, OutOfBandPolicy, ShapeFamily};
    use poise_core::types::ExchangeRules;
    use poise_engine::track::{TrackId, Instrument, Venue};
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        ClockPort, TrackReadRepositoryPort, OrderStatus, StateRepositoryPort, StoredTrackSnapshot,
    };
    use poise_protocol::{
        ExecutionIntentView, ExecutionSlotPhaseView, ExecutionStatusView, GridCommandAccepted,
        GridCommandRequest, GridCommandType, GridDetailView, GridListResponse, GridStatus,
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
            .save_transition("btc-core", &snapshot, &[], &[])
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

        build_server_state(
            write_service,
            state_repository,
            Arc::new(TrackQueryService::new(read_repository)),
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
        let grid = manager
            .get_track("btc-core")
            .expect("grid should still exist")
            .clone();
        let mut snapshot = grid.snapshot();
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
    async fn list_grids_returns_grid_list_response() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/grids")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: GridListResponse = serde_json::from_slice(&body).unwrap();

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
    async fn get_grid_detail_returns_projected_detail() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/grids/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: GridDetailView = serde_json::from_slice(&body).unwrap();
        let payload_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.identity.id, "btc-core");
        assert_eq!(payload.identity.instrument.symbol, "BTCUSDT");
        assert!((payload.statistics.realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((payload.statistics.total_pnl - 1245.3).abs() < f64::EPSILON);
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
                    .uri("/grids/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&GridCommandRequest {
                            command: GridCommandType::Pause,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: GridCommandAccepted = serde_json::from_slice(&body).unwrap();

        assert!(payload.accepted);
        assert_eq!(payload.grid_id, "btc-core");
        assert_eq!(payload.command, GridCommandType::Pause);
    }

    #[tokio::test]
    async fn submit_command_accepts_flatten() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&GridCommandRequest {
                            command: GridCommandType::Flatten,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: GridCommandAccepted = serde_json::from_slice(&body).unwrap();
        assert!(payload.accepted);
        assert_eq!(payload.grid_id, "btc-core");
        assert_eq!(payload.command, GridCommandType::Flatten);
    }

    #[tokio::test]
    async fn resume_command_rejects_non_paused_grid() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&GridCommandRequest {
                            command: GridCommandType::Resume,
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
        let app = router(build_server_state(
            Arc::new(TrackWriteService::new(
                manager,
                state_repository.clone(),
                notifications,
            )),
            state_repository,
            Arc::new(TrackQueryService::new(
                repository as Arc<dyn TrackReadRepositoryPort>,
            )),
            Arc::new(TrackProjector::new()),
        ));

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&GridCommandRequest {
                            command: GridCommandType::Pause,
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
                    .uri("/grids/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: GridDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status.lifecycle.status, GridStatus::Active);
    }

    #[tokio::test]
    async fn pause_command_updates_detail_status() {
        let app = router(app_state().await);

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&GridCommandRequest {
                            command: GridCommandType::Pause,
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
                    .uri("/grids/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: GridDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status.lifecycle.status, GridStatus::Paused);
        assert_eq!(payload.position.target_exposure, None);
    }

    #[tokio::test]
    async fn resume_command_reactivates_paused_grid() {
        let app = router(app_state().await);

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&GridCommandRequest {
                            command: GridCommandType::Pause,
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
                    .uri("/grids/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&GridCommandRequest {
                            command: GridCommandType::Resume,
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
                    .uri("/grids/btc-core")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(detail.into_body(), usize::MAX).await.unwrap();
        let payload: GridDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status.lifecycle.status, GridStatus::Active);
    }

    #[tokio::test]
    async fn get_grid_detail_returns_404_for_missing_grid() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/grids/ETHUSDT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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

        async fn list_events(
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

        async fn list_pending_submit_effects_for_track(
            &self,
            _grid_id: &TrackId,
        ) -> anyhow::Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for FailingRepository {
        async fn list_track_snapshots(&self) -> anyhow::Result<Vec<StoredTrackSnapshot>> {
            Ok(self.snapshots.lock().unwrap().values().cloned().collect())
        }

        async fn load_track_snapshot(
            &self,
            grid_id: &TrackId,
        ) -> anyhow::Result<Option<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(grid_id.as_str())
                .cloned())
        }

        async fn list_recent_track_events(
            &self,
            _grid_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<poise_engine::ports::StoredDomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            _grid_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }
    }
}
