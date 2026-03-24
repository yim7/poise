use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use grid_engine::instance::{InstanceStatus, PendingOrder, StrategyInstance};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

use crate::assembly::{AppState, MutateAndPersistError, mutate_instance_and_persist};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceSummary {
    pub id: String,
    pub symbol: String,
    pub status: InstanceStatus,
    pub last_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// HTTP DTO: exposes client-facing snapshot fields, but not internal risk bookkeeping.
pub struct InstanceSnapshot {
    pub id: String,
    pub symbol: String,
    pub status: InstanceStatus,
    pub current_exposure: f64,
    pub target_exposure: Option<f64>,
    pub last_price: Option<f64>,
    pub pending_order: Option<PendingOrder>,
    pub config: grid_core::strategy::GridConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CommandRequest {
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedCommand {
    Pause,
    Resume,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResponse {
    pub instance_id: String,
    pub command: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ErrorResponse {
    error: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/instances", get(list_instances))
        .route("/instances/:id/snapshot", get(get_snapshot))
        .route("/instances/:id/commands", post(submit_command))
        .route("/ws", get(crate::websocket::ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn list_instances(State(state): State<AppState>) -> Json<Vec<InstanceSummary>> {
    let manager = state.manager.read().await;
    let items = manager
        .list_instances()
        .into_iter()
        .map(InstanceSummary::from)
        .collect();
    Json(items)
}

async fn get_snapshot(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<InstanceSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    let manager = state.manager.read().await;
    let instance = manager
        .get_instance(&id)
        .ok_or_else(|| not_found(format!("instance `{id}` not found")))?;
    Ok(Json(InstanceSnapshot::from(instance)))
}

async fn submit_command(
    Path(id): Path<String>,
    State(state): State<AppState>,
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

    let manager = state.manager.read().await;
    if manager.get_instance(&id).is_none() {
        return Err(not_found(format!("instance `{id}` not found")));
    }
    drop(manager);

    match mutate_instance_and_persist(&state, &id, |manager| match command {
        SupportedCommand::Pause => manager.pause_instance(&id),
        SupportedCommand::Resume => manager.resume_instance(&id),
    })
    .await
    {
        Ok(()) => {}
        Err(MutateAndPersistError::Mutation(error)) => {
            return Err(bad_request(error.to_string()));
        }
        Err(MutateAndPersistError::Persistence(error)) => {
            return Err(internal_error(error.to_string()));
        }
    }

    Ok(Json(CommandResponse {
        instance_id: id,
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

impl From<&StrategyInstance> for InstanceSummary {
    fn from(value: &StrategyInstance) -> Self {
        Self {
            id: value.id.clone(),
            symbol: value.symbol.clone(),
            status: value.status.clone(),
            last_price: value.last_price,
        }
    }
}

impl From<&StrategyInstance> for InstanceSnapshot {
    fn from(value: &StrategyInstance) -> Self {
        Self {
            id: value.id.clone(),
            symbol: value.symbol.clone(),
            status: value.status.clone(),
            current_exposure: value.current_exposure.0,
            target_exposure: value.target_exposure.as_ref().map(|exposure| exposure.0),
            last_price: value.last_price,
            pending_order: value.pending_order.clone(),
            config: value.config.clone(),
        }
    }
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
    use grid_engine::instance::{InstanceStatus, PendingOrder};
    use grid_engine::manager::InstanceManager;
    use grid_engine::ports::{
        ClockPort, ExchangeInfo, ExchangePort, OpenOrder, OrderReceipt, OrderRequest,
        PersistencePort, Position,
    };
    use serde_json::json;
    use tower::ServiceExt;

    use crate::assembly::AppState;
    use crate::websocket::WsEvent;

    use super::{CommandResponse, InstanceSnapshot, InstanceSummary, router};

    fn test_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.0,
            quantity_step: 0.0,
            min_qty: 0.0,
            min_notional: 0.0,
        }
    }

    struct FakeExchange;

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, _req: OrderRequest) -> anyhow::Result<OrderReceipt> {
            unreachable!()
        }
        async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        async fn cancel_all(&self, _symbol: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        async fn get_position(&self, _symbol: &str) -> anyhow::Result<Position> {
            unreachable!()
        }
        async fn get_open_orders(&self, _symbol: &str) -> anyhow::Result<Vec<OpenOrder>> {
            unreachable!()
        }
        async fn get_exchange_info(&self, _symbol: &str) -> anyhow::Result<ExchangeInfo> {
            unreachable!()
        }
    }

    struct FakePersistence;

    #[async_trait::async_trait]
    impl PersistencePort for FakePersistence {
        async fn save_instance_state(
            &self,
            _id: &str,
            _state: &grid_engine::ports::InstanceSnapshot,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn load_instance_state(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<grid_engine::ports::InstanceSnapshot>> {
            Ok(None)
        }
    }

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }

    fn app_state() -> AppState {
        app_state_with_persistence(Arc::new(FakePersistence))
    }

    fn app_state_with_persistence(persistence: Arc<dyn PersistencePort>) -> AppState {
        let mut manager = InstanceManager::new(
            Arc::new(FakeExchange),
            Arc::clone(&persistence),
            Arc::new(FakeClock),
        );
        manager
            .add_instance(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                GridConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_capacity: 8.0,
                    short_capacity: 8.0,
                    capacity_notional: 375.0,
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
            last_price: 95.0,
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
            status: "NEW".into(),
        };
        manager
            .restore_instance_state(&grid_engine::ports::InstanceSnapshot {
                id: strategy.id.clone(),
                symbol: strategy.symbol.clone(),
                config: strategy.config.clone(),
                status: strategy.status.clone(),
                current_exposure: strategy.current_exposure.clone(),
                target_exposure: strategy.target_exposure.clone(),
                pending_order: Some(pending_order),
                risk_state: strategy.risk_state.clone(),
                last_price: strategy.last_price,
                out_of_band_since: strategy.out_of_band_since,
            })
            .unwrap();
        let (events, _) = tokio::sync::broadcast::channel::<WsEvent>(16);

        AppState {
            manager: Arc::new(tokio::sync::RwLock::new(manager)),
            persistence,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
            events,
        }
    }

    #[tokio::test]
    async fn list_instances_returns_registered_instances() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .uri("/instances")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: Vec<InstanceSummary> = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0].id, "BTCUSDT");
    }

    #[tokio::test]
    async fn get_snapshot_returns_instance_snapshot() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .uri("/instances/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: InstanceSnapshot = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.id, "BTCUSDT");
        assert_eq!(payload.last_price, Some(95.0));
        assert_eq!(payload.target_exposure, Some(4.0));
        assert!(payload.pending_order.is_some());
    }

    #[tokio::test]
    async fn get_snapshot_serializes_pending_order_side_as_snake_case() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .uri("/instances/BTCUSDT/snapshot")
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
                    .uri("/instances/BTCUSDT/commands")
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
                    .uri("/instances/BTCUSDT/commands")
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
                    .uri("/instances/BTCUSDT/commands")
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
                    .uri("/instances/BTCUSDT/commands")
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
                    .uri("/instances/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(snapshot.into_body(), usize::MAX).await.unwrap();
        let payload: InstanceSnapshot = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status, InstanceStatus::Active);
    }

    #[tokio::test]
    async fn pause_command_updates_snapshot_status() {
        let app = router(app_state());

        let pause = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/instances/BTCUSDT/commands")
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
                    .uri("/instances/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(snapshot.into_body(), usize::MAX).await.unwrap();
        let payload: InstanceSnapshot = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status, InstanceStatus::Paused);
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
                    .uri("/instances/BTCUSDT/commands")
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
                    .uri("/instances/BTCUSDT/commands")
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
                    .uri("/instances/BTCUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = to_bytes(snapshot.into_body(), usize::MAX).await.unwrap();
        let payload: InstanceSnapshot = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status, InstanceStatus::Active);
    }

    #[tokio::test]
    async fn get_snapshot_returns_404_for_missing_instance() {
        let response = router(app_state())
            .oneshot(
                Request::builder()
                    .uri("/instances/ETHUSDT/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    struct FailingPersistence;

    #[async_trait::async_trait]
    impl PersistencePort for FailingPersistence {
        async fn save_instance_state(
            &self,
            _id: &str,
            _state: &grid_engine::ports::InstanceSnapshot,
        ) -> anyhow::Result<()> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn load_instance_state(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<grid_engine::ports::InstanceSnapshot>> {
            Ok(None)
        }
    }
}
