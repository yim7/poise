use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
pub use grid_protocol::{CommandRequest, CommandResponse, GridSnapshot, GridSummary};
use serde::Serialize;
use tower_http::cors::CorsLayer;

use crate::application::GridMutationError;
use crate::assembly::ServerState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedCommand {
    Pause,
    Resume,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ErrorResponse {
    error: String,
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/grids", get(list_grids))
        .route("/grids/:id/snapshot", get(get_snapshot))
        .route("/grids/:id/commands", post(submit_command))
        .route("/ws", get(crate::websocket::ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn list_grids(State(state): State<ServerState>) -> Json<Vec<GridSummary>> {
    Json(state.service.list_grid_summaries().await)
}

async fn get_snapshot(
    Path(id): Path<String>,
    State(state): State<ServerState>,
) -> Result<Json<GridSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    let snapshot = state
        .service
        .grid_snapshot(&id)
        .await
        .ok_or_else(|| not_found(format!("grid `{id}` not found")))?;
    Ok(Json(snapshot))
}

async fn submit_command(
    Path(id): Path<String>,
    State(state): State<ServerState>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<CommandResponse>, (StatusCode, Json<ErrorResponse>)> {
    if request.command.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "command must not be empty".to_string(),
            }),
        ));
    }

    let command = SupportedCommand::try_from(request.command.as_str()).map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: message }),
        )
    })?;

    if !state.service.has_grid(&id).await {
        return Err(not_found(format!("grid `{id}` not found")));
    }

    match state
        .service
        .mutate_grid(&id, |manager| match command {
            SupportedCommand::Pause => manager.pause_instance(&id),
            SupportedCommand::Resume => manager.resume_instance(&id),
        })
        .await
    {
        Ok(()) => {}
        Err(GridMutationError::Mutation(error)) => {
            return Err(bad_request(error.to_string()));
        }
        Err(GridMutationError::Persistence(error)) => {
            return Err(internal_error(error.to_string()));
        }
    }

    Ok(Json(CommandResponse {
        grid_id: id,
        command: request.command,
        accepted: true,
    }))
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
impl TryFrom<&str> for SupportedCommand {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pause" => Ok(Self::Pause),
            "resume" => Ok(Self::Resume),
            other => Err(format!(
                "unsupported command `{other}`; supported commands: pause, resume"
            )),
        }
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
    use grid_engine::instance::PendingOrder;
    use grid_engine::manager::InstanceManager;
    use grid_engine::ports::{ClockPort, OrderStatus, StateRepositoryPort};
    use grid_protocol::GridStatus;
    use serde_json::json;
    use tower::ServiceExt;

    use crate::application::GridPlatformService;
    use crate::assembly::ServerState;
    use crate::websocket::WsEvent;

    use super::{CommandResponse, GridSnapshot, GridSummary, router};

    fn test_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.0,
            quantity_step: 0.0,
            min_qty: 0.0,
            min_notional: 0.0,
        }
    }

    struct FakePersistence;

    #[async_trait::async_trait]
    impl StateRepositoryPort for FakePersistence {
        async fn save_transition(
            &self,
            _id: &str,
            _state: &grid_engine::ports::GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
        ) -> anyhow::Result<()> {
            Ok(())
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
    }

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }

    fn app_state() -> ServerState {
        app_state_with_persistence(Arc::new(FakePersistence))
    }

    fn app_state_with_persistence(repository: Arc<dyn StateRepositoryPort>) -> ServerState {
        let mut manager = InstanceManager::new(Arc::new(FakeClock));
        manager
            .add_grid(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
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
        let tick = grid_engine::ports::PriceTick {
            symbol: "BTCUSDT".into(),
            reference_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };
        manager.on_price_tick(&tick).unwrap();
        let instance = manager.get_instance("BTCUSDT").unwrap().id.clone();
        let strategy = manager.get_instance(&instance).unwrap();
        let strategy = manager
            .get_instance(&strategy.id)
            .expect("instance should still exist");
        let pending_order = PendingOrder {
            symbol: "BTCUSDT".into(),
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: Exposure(4.0),
            status: OrderStatus::New,
        };
        manager
            .restore_instance_state(&grid_engine::ports::GridSnapshot {
                id: strategy.id.clone(),
                symbol: strategy.symbol.clone(),
                config: strategy.config.clone(),
                status: strategy.status.clone(),
                current_exposure: strategy.current_exposure.clone(),
                target_exposure: strategy.target_exposure.clone(),
                pending_order: Some(pending_order),
                risk_state: strategy.risk_state.clone(),
                reference_price: strategy.reference_price,
                out_of_band_since: strategy.out_of_band_since,
            })
            .unwrap();
        let (events, _) = tokio::sync::broadcast::channel::<WsEvent>(16);

        ServerState {
            service: Arc::new(GridPlatformService::new(manager, repository, events)),
        }
    }

    #[tokio::test]
    async fn list_instances_returns_registered_instances() {
        let response = router(app_state())
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
        let payload: Vec<GridSummary> = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0].id, "BTCUSDT");
    }

    #[tokio::test]
    async fn get_snapshot_returns_instance_snapshot() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .uri("/grids/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: GridSnapshot = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.id, "BTCUSDT");
        assert_eq!(payload.reference_price, Some(95.0));
        assert_eq!(payload.target_exposure, Some(4.0));
        assert!(payload.pending_order.is_some());
    }

    #[tokio::test]
    async fn get_snapshot_serializes_pending_order_side_as_snake_case() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .uri("/grids/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload["pending_order"]["side"], "buy");
    }

    #[tokio::test]
    async fn submit_command_accepts_valid_command() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/BTCUSDT/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "command": "pause" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: CommandResponse = serde_json::from_slice(&body).unwrap();

        assert!(payload.accepted);
        assert_eq!(payload.command, "pause");
    }

    #[tokio::test]
    async fn submit_command_rejects_unknown_command() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/BTCUSDT/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "command": "flatten-now" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn resume_command_rejects_non_paused_instance() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/BTCUSDT/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "command": "resume" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn submit_command_rolls_back_state_when_persistence_fails() {
        let app = router(app_state_with_persistence(Arc::new(FailingPersistence)));

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/BTCUSDT/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "command": "pause" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let snapshot = app
            .oneshot(
                Request::builder()
                    .uri("/grids/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(snapshot.into_body(), usize::MAX).await.unwrap();
        let payload: GridSnapshot = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status, GridStatus::Active);
    }

    #[tokio::test]
    async fn pause_command_updates_snapshot_status() {
        let app = router(app_state());

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/BTCUSDT/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "command": "pause" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pause.status(), StatusCode::OK);

        let snapshot = app
            .oneshot(
                Request::builder()
                    .uri("/grids/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(snapshot.into_body(), usize::MAX).await.unwrap();
        let payload: GridSnapshot = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status, GridStatus::Paused);
        assert_eq!(payload.target_exposure, None);
    }

    #[tokio::test]
    async fn resume_command_reactivates_paused_instance() {
        let app = router(app_state());

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/grids/BTCUSDT/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "command": "pause" })).unwrap(),
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
                    .uri("/grids/BTCUSDT/commands")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({ "command": "resume" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resume.status(), StatusCode::OK);

        let snapshot = app
            .oneshot(
                Request::builder()
                    .uri("/grids/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(snapshot.into_body(), usize::MAX).await.unwrap();
        let payload: GridSnapshot = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status, GridStatus::Active);
    }

    #[tokio::test]
    async fn get_snapshot_returns_404_for_missing_instance() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .uri("/grids/ETHUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    struct FailingPersistence;

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailingPersistence {
        async fn save_transition(
            &self,
            _id: &str,
            _state: &grid_engine::ports::GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
        ) -> anyhow::Result<()> {
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
    }
}
