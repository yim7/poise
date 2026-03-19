use axum::{
    Json, Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

use crate::{
    Application,
    application::success,
    protocol::{
        CommandAccepted, CommandRequest, CommandType, HttpErrorDetail, HttpErrorEnvelope,
        HttpSuccessEnvelope, PROTOCOL_VERSION, PriceUpdated, RuntimeSnapshot, ServerEnvelope,
    },
};

pub fn build_app(application: Application) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/runtime/snapshot", get(runtime_snapshot))
        .route("/orders/open", get(open_orders))
        .route("/fills/recent", get(recent_fills))
        .route("/risk/events", get(risk_events))
        .route("/system/events", get(system_events))
        .route("/commands/pause", post(pause))
        .route("/commands/resume", post(resume))
        .route("/commands/cancel-all", post(cancel_all))
        .route("/commands/flatten-now", post(flatten_now))
        .route(
            "/commands/shutdown-after-flatten",
            post(shutdown_after_flatten),
        )
        .route("/__test__/emit-price-tick", post(emit_price_tick))
        .route("/ws", get(ws_events))
        .with_state(application)
}

#[derive(Debug, Serialize)]
struct HealthResponse<'a> {
    service: &'a str,
    status: &'a str,
}

#[derive(Debug)]
struct ApiError {
    code: &'static str,
    message: String,
}

impl ApiError {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: "service_unavailable",
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = axum::http::StatusCode::SERVICE_UNAVAILABLE;
        let body = Json(HttpErrorEnvelope {
            version: PROTOCOL_VERSION.into(),
            status: "error".into(),
            error: HttpErrorDetail {
                code: self.code.into(),
                message: self.message,
                details: None,
            },
        });
        (status, body).into_response()
    }
}

async fn healthz() -> Json<HealthResponse<'static>> {
    Json(HealthResponse {
        service: "grid-platform-service",
        status: "ok",
    })
}

async fn runtime_snapshot(
    State(application): State<Application>,
) -> Json<HttpSuccessEnvelope<RuntimeSnapshot>> {
    Json(success(application.snapshot()))
}

async fn open_orders(
    State(application): State<Application>,
) -> Json<HttpSuccessEnvelope<Vec<crate::protocol::OpenOrder>>> {
    Json(success(application.open_orders()))
}

async fn recent_fills(
    State(application): State<Application>,
) -> Json<HttpSuccessEnvelope<Vec<crate::protocol::RecentFill>>> {
    Json(success(application.recent_fills()))
}

async fn risk_events(
    State(application): State<Application>,
) -> Json<HttpSuccessEnvelope<Vec<crate::protocol::RiskEvent>>> {
    Json(success(application.risk_events()))
}

async fn system_events(
    State(application): State<Application>,
) -> Json<HttpSuccessEnvelope<Vec<crate::protocol::SystemEvent>>> {
    Json(success(application.system_events()))
}

async fn pause(
    State(application): State<Application>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(application, CommandType::Pause, request).await
}

async fn resume(
    State(application): State<Application>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(application, CommandType::Resume, request).await
}

async fn cancel_all(
    State(application): State<Application>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(application, CommandType::CancelAll, request).await
}

async fn flatten_now(
    State(application): State<Application>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(application, CommandType::FlattenNow, request).await
}

async fn shutdown_after_flatten(
    State(application): State<Application>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(application, CommandType::ShutdownAfterFlatten, request).await
}

async fn emit_price_tick(
    State(application): State<Application>,
) -> Result<Json<HttpSuccessEnvelope<PriceUpdated>>, ApiError> {
    let tick = application
        .emit_price_tick()
        .await
        .map_err(|error| ApiError::unavailable(error.to_string()))?;
    Ok(Json(success(tick)))
}

async fn issue_command(
    application: Application,
    command: CommandType,
    request: CommandRequest,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    let accepted = application
        .submit_command(command, request)
        .await
        .map_err(|error| ApiError::unavailable(error.to_string()))?;
    Ok(Json(success(accepted)))
}

async fn ws_events(
    ws: WebSocketUpgrade,
    State(application): State<Application>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| websocket_session(socket, application))
}

async fn websocket_session(mut socket: WebSocket, application: Application) {
    let mut subscription = application.subscribe_runtime_stream();
    if send_ws_event(&mut socket, &subscription.initial_snapshot)
        .await
        .is_err()
    {
        return;
    }

    while let Ok(event) = subscription.receiver.recv().await {
        if event
            .sequence
            .is_some_and(|sequence| sequence <= subscription.snapshot_sequence)
        {
            continue;
        }
        if send_ws_event(&mut socket, &event).await.is_err() {
            break;
        }
    }
}

async fn send_ws_event(socket: &mut WebSocket, event: &ServerEnvelope) -> Result<(), ()> {
    let payload = serde_json::to_string(event).map_err(|_| ())?;
    socket
        .send(Message::Text(payload.into()))
        .await
        .map_err(|_| ())
}
