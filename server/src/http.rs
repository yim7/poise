use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use grid_engine::instance::{InstanceStatus, StrategyInstance};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

use crate::assembly::AppState;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceSummary {
    pub id: String,
    pub symbol: String,
    pub status: InstanceStatus,
    pub last_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceSnapshot {
    pub id: String,
    pub symbol: String,
    pub status: InstanceStatus,
    pub current_exposure: f64,
    pub last_price: Option<f64>,
    pub config: grid_core::strategy::GridConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CommandRequest {
    pub command: String,
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

    let manager = state.manager.read().await;
    if manager.get_instance(&id).is_none() {
        return Err(not_found(format!("instance `{id}` not found")));
    }

    Ok(Json(CommandResponse {
        instance_id: id,
        command: request.command,
        accepted: true,
    }))
}

fn not_found(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
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
            last_price: value.last_price,
            config: value.config.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use chrono::Utc;
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
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

    struct FakeExchange;

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, _req: OrderRequest) -> anyhow::Result<OrderReceipt> {
            unreachable!()
        }
        async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        async fn cancel_all(&self, _symbol: &str) -> anyhow::Result<Vec<String>> {
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
        let mut manager = InstanceManager::new(
            Arc::new(FakeExchange),
            Arc::new(FakePersistence),
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
                    max_notional: 375.0,
                    daily_loss_limit: -100.0,
                    stop_loss_pct: 10.0,
                },
            )
            .unwrap();
        let tick = grid_engine::ports::PriceTick {
            symbol: "BTCUSDT".into(),
            last_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };
        let _ = manager.on_price_tick(&tick);
        let (events, _) = tokio::sync::broadcast::channel::<WsEvent>(16);

        AppState {
            manager: Arc::new(tokio::sync::RwLock::new(manager)),
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
}
