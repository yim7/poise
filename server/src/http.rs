use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use grid_engine::command::GridCommand;
use grid_engine::grid::GridId;
use grid_protocol::{
    GridCommandAccepted, GridCommandRequest, GridCommandType, GridDetailView, GridListResponse,
};
use serde::Serialize;
use tower_http::cors::CorsLayer;

use crate::assembly::ServerState;
use crate::write_service::GridMutationError;

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ErrorResponse {
    error: String,
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/grids", get(list_grids))
        .route("/grids/:id", get(get_grid_detail))
        .route("/grids/:id/commands", post(submit_command))
        .route("/ws", get(crate::websocket::ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn list_grids(
    State(state): State<ServerState>,
) -> Result<Json<GridListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let sources = state
        .query_service
        .list_grid_sources()
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
    let grid_id = GridId::new(id.clone());
    let source = state
        .query_service
        .load_detail_source(&grid_id)
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
    if !state.write_service.has_grid(&id).await {
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

fn map_command(command: GridCommandType) -> Result<GridCommand, (StatusCode, Json<ErrorResponse>)> {
    match command {
        GridCommandType::Pause => Ok(GridCommand::Pause),
        GridCommandType::Resume => Ok(GridCommand::Resume),
        GridCommandType::Terminate | GridCommandType::Flatten => Err(bad_request(format!(
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
    match error.downcast::<GridMutationError>() {
        Ok(GridMutationError::Mutation(error)) => bad_request(error.to_string()),
        Ok(GridMutationError::Persistence(error)) => internal_error(error.to_string()),
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
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{ExchangeRules, Exposure};
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::GridManager;
    use grid_engine::ports::{
        ClockPort, GridReadRepositoryPort, OrderStatus, StateRepositoryPort, StoredGridSnapshot,
    };
    use grid_engine::runtime::PendingOrder;
    use grid_protocol::{
        GridCommandAccepted, GridCommandRequest, GridCommandType, GridDetailView, GridListResponse,
        GridStatus,
    };
    use grid_storage::sqlite::SqliteStorage;
    use tower::ServiceExt;

    use crate::assembly::{ServerState, build_server_state};
    use crate::effect_service::EffectService;
    use crate::notifications::GridInternalNotification;
    use crate::projector::GridProjector;
    use crate::query_service::GridQueryService;
    use crate::write_service::GridWriteService;

    use super::router;

    fn test_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.0,
            quantity_step: 0.0,
            min_qty: 0.0,
            min_notional: 0.0,
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
        R: StateRepositoryPort + GridReadRepositoryPort + 'static,
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
        let (notifications, _) = tokio::sync::broadcast::channel::<GridInternalNotification>(16);
        let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
        let read_repository: Arc<dyn GridReadRepositoryPort> = repository;
        let effect_service = Arc::new(EffectService::new(
            state_repository.clone(),
            notifications.clone(),
        ));
        let write_service = Arc::new(GridWriteService::new(
            manager,
            state_repository,
            notifications,
        ));

        build_server_state(
            write_service,
            effect_service,
            Arc::new(GridQueryService::new(read_repository)),
            Arc::new(GridProjector::new()),
        )
    }

    fn test_manager() -> GridManager {
        let mut manager = GridManager::new(Arc::new(FakeClock));
        manager
            .add_grid(
                GridId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                GridConfig {
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
                &GridId::new("btc-core"),
                grid_engine::observation::GridObservation::Market(
                    grid_engine::observation::MarketObservation {
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();
        let grid = manager
            .get_grid("btc-core")
            .expect("grid should still exist")
            .clone();
        let pending_order = PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: Exposure(4.0),
            status: OrderStatus::New,
        };
        manager
            .restore_grid_state(&grid_engine::ports::GridSnapshot {
                grid_id: grid.id,
                instrument: grid.instrument,
                config: grid.config,
                status: grid.status,
                current_exposure: grid.current_exposure,
                target_exposure: grid.target_exposure,
                pending_order: Some(pending_order),
                risk: grid.risk_state,
                observed: grid_engine::snapshot::ObservedState {
                    reference_price: grid.reference_price,
                    out_of_band_since: grid.out_of_band_since,
                },
            })
            .unwrap();
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
        assert!(!payload.available_commands.is_empty());
        assert_eq!(
            payload
                .execution
                .pending_order
                .as_ref()
                .unwrap()
                .order_id
                .as_deref(),
            Some("order-1")
        );
        assert!(
            payload_json["execution"]["pending_order"]
                .get("client_order_id")
                .is_none()
        );
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
    async fn submit_command_rejects_unimplemented_command() {
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

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"], "command `flatten` is not implemented");
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
        let (notifications, _) = tokio::sync::broadcast::channel::<GridInternalNotification>(16);
        let effect_service = Arc::new(EffectService::new(
            repository.clone() as Arc<dyn StateRepositoryPort>,
            notifications.clone(),
        ));
        let app = router(build_server_state(
            Arc::new(GridWriteService::new(
                manager,
                repository.clone() as Arc<dyn StateRepositoryPort>,
                notifications,
            )),
            effect_service,
            Arc::new(GridQueryService::new(
                repository as Arc<dyn GridReadRepositoryPort>,
            )),
            Arc::new(GridProjector::new()),
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
        snapshots: std::sync::Mutex<std::collections::HashMap<String, StoredGridSnapshot>>,
    }

    impl FailingRepository {
        fn seed_snapshot(&self, snapshot: grid_engine::ports::GridSnapshot) {
            self.snapshots.lock().unwrap().insert(
                snapshot.grid_id.as_str().to_string(),
                StoredGridSnapshot {
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
            _state: &grid_engine::ports::GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
            _effects: &[grid_engine::transition::GridEffect],
            _effect_status_update: Option<&grid_engine::ports::EffectStatusUpdate>,
        ) -> anyhow::Result<grid_engine::ports::CommittedGridWrite> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn load_grid_state(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<grid_engine::ports::GridSnapshot>> {
            Ok(None)
        }

        async fn list_events(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(
            &self,
        ) -> anyhow::Result<Vec<grid_engine::ports::PersistedGridEffect>> {
            Ok(Vec::new())
        }

        async fn mark_effect_executing(&self, _effect_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn mark_effect_succeeded(&self, _effect_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn mark_effect_superseded(&self, _effect_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn mark_effect_failed(&self, _effect_id: &str, _error: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl GridReadRepositoryPort for FailingRepository {
        async fn list_grid_snapshots(&self) -> anyhow::Result<Vec<StoredGridSnapshot>> {
            Ok(self.snapshots.lock().unwrap().values().cloned().collect())
        }

        async fn load_grid_snapshot(
            &self,
            grid_id: &GridId,
        ) -> anyhow::Result<Option<StoredGridSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(grid_id.as_str())
                .cloned())
        }

        async fn list_recent_grid_events(
            &self,
            _grid_id: &GridId,
            _limit: usize,
        ) -> anyhow::Result<Vec<grid_engine::ports::StoredDomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_grid_effects(
            &self,
            _grid_id: &GridId,
            _limit: usize,
        ) -> anyhow::Result<Vec<grid_engine::ports::PersistedGridEffect>> {
            Ok(Vec::new())
        }
    }
}
