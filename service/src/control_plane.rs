use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::{
    Application,
    application::success,
    protocol::{
        AlertsFilters, AlertsQueryResult, CommandAccepted, CommandRequest, CommandType,
        CommandsFilters, CommandsQueryResult, ControlPlaneCapabilities, FillsFilters,
        FillsQueryResult, HttpErrorDetail, HttpErrorEnvelope, HttpSuccessEnvelope, OrdersFilters,
        OrdersQueryResult, PROTOCOL_VERSION, PriceUpdated, RuntimeQueryResult, RuntimeSnapshot,
        ServerEnvelope,
    },
    registry::{ApplicationRegistry, InstancesDirectory},
};

pub fn build_app(registry: impl Into<ApplicationRegistry>) -> Router {
    let registry = registry.into();
    Router::new()
        .route("/healthz", get(healthz))
        .route("/instances", get(instances))
        .route("/runtime/snapshot", get(runtime_snapshot))
        .route("/orders/open", get(open_orders))
        .route("/fills/recent", get(recent_fills))
        .route("/risk/events", get(risk_events))
        .route("/system/events", get(system_events))
        .route("/query/runtime", get(query_runtime))
        .route("/query/orders", get(query_orders))
        .route("/query/fills", get(query_fills))
        .route("/query/alerts", get(query_alerts))
        .route("/query/commands", get(query_commands))
        .route(
            "/control-plane/capabilities",
            get(control_plane_capabilities),
        )
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
        .route(
            "/instances/{symbol}/runtime/snapshot",
            get(runtime_snapshot_for_instance),
        )
        .route(
            "/instances/{symbol}/orders/open",
            get(open_orders_for_instance),
        )
        .route(
            "/instances/{symbol}/fills/recent",
            get(recent_fills_for_instance),
        )
        .route(
            "/instances/{symbol}/risk/events",
            get(risk_events_for_instance),
        )
        .route(
            "/instances/{symbol}/system/events",
            get(system_events_for_instance),
        )
        .route(
            "/instances/{symbol}/query/runtime",
            get(query_runtime_for_instance),
        )
        .route(
            "/instances/{symbol}/query/orders",
            get(query_orders_for_instance),
        )
        .route(
            "/instances/{symbol}/query/fills",
            get(query_fills_for_instance),
        )
        .route(
            "/instances/{symbol}/query/alerts",
            get(query_alerts_for_instance),
        )
        .route(
            "/instances/{symbol}/query/commands",
            get(query_commands_for_instance),
        )
        .route(
            "/instances/{symbol}/control-plane/capabilities",
            get(control_plane_capabilities_for_instance),
        )
        .route(
            "/instances/{symbol}/commands/pause",
            post(pause_for_instance),
        )
        .route(
            "/instances/{symbol}/commands/resume",
            post(resume_for_instance),
        )
        .route(
            "/instances/{symbol}/commands/cancel-all",
            post(cancel_all_for_instance),
        )
        .route(
            "/instances/{symbol}/commands/flatten-now",
            post(flatten_now_for_instance),
        )
        .route(
            "/instances/{symbol}/commands/shutdown-after-flatten",
            post(shutdown_after_flatten_for_instance),
        )
        .route(
            "/instances/{symbol}/__test__/emit-price-tick",
            post(emit_price_tick_for_instance),
        )
        .route("/instances/{symbol}/ws", get(ws_events_for_instance))
        .with_state(registry)
}

#[derive(Debug, Serialize)]
struct HealthResponse<'a> {
    service: &'a str,
    status: &'a str,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "service_unavailable",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "instance_not_found",
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(HttpErrorEnvelope {
            version: PROTOCOL_VERSION.into(),
            status: "error".into(),
            error: HttpErrorDetail {
                code: self.code.into(),
                message: self.message,
                details: None,
            },
        });
        (self.status, body).into_response()
    }
}

async fn healthz() -> Json<HealthResponse<'static>> {
    Json(HealthResponse {
        service: "grid-platform-service",
        status: "ok",
    })
}

async fn instances(
    State(registry): State<ApplicationRegistry>,
) -> Json<HttpSuccessEnvelope<InstancesDirectory>> {
    Json(success(registry.directory()))
}

fn default_application(registry: &ApplicationRegistry) -> Application {
    registry.default_application()
}

fn application_for_symbol(
    registry: &ApplicationRegistry,
    symbol: &str,
) -> Result<Application, ApiError> {
    registry
        .application(symbol)
        .ok_or_else(|| ApiError::not_found(format!("instance `{symbol}` was not found")))
}

async fn runtime_snapshot(
    State(registry): State<ApplicationRegistry>,
) -> Json<HttpSuccessEnvelope<RuntimeSnapshot>> {
    Json(success(default_application(&registry).snapshot()))
}

async fn runtime_snapshot_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<RuntimeSnapshot>>, ApiError> {
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.snapshot(),
    )))
}

async fn open_orders(
    State(registry): State<ApplicationRegistry>,
) -> Json<HttpSuccessEnvelope<Vec<crate::protocol::OpenOrder>>> {
    Json(success(default_application(&registry).open_orders()))
}

async fn open_orders_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<Vec<crate::protocol::OpenOrder>>>, ApiError> {
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.open_orders(),
    )))
}

async fn recent_fills(
    State(registry): State<ApplicationRegistry>,
) -> Json<HttpSuccessEnvelope<Vec<crate::protocol::RecentFill>>> {
    Json(success(default_application(&registry).recent_fills()))
}

async fn recent_fills_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<Vec<crate::protocol::RecentFill>>>, ApiError> {
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.recent_fills(),
    )))
}

async fn risk_events(
    State(registry): State<ApplicationRegistry>,
) -> Json<HttpSuccessEnvelope<Vec<crate::protocol::RiskEvent>>> {
    Json(success(default_application(&registry).risk_events()))
}

async fn risk_events_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<Vec<crate::protocol::RiskEvent>>>, ApiError> {
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.risk_events(),
    )))
}

async fn system_events(
    State(registry): State<ApplicationRegistry>,
) -> Json<HttpSuccessEnvelope<Vec<crate::protocol::SystemEvent>>> {
    Json(success(default_application(&registry).system_events()))
}

async fn system_events_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<Vec<crate::protocol::SystemEvent>>>, ApiError> {
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.system_events(),
    )))
}

#[derive(Debug, Default, Deserialize)]
struct OrdersQueryParams {
    page: Option<usize>,
    per_page: Option<usize>,
    side: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FillsQueryParams {
    page: Option<usize>,
    per_page: Option<usize>,
    side: Option<String>,
    order_id: Option<String>,
    client_order_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AlertsQueryParams {
    page: Option<usize>,
    per_page: Option<usize>,
    category: Option<String>,
    severity: Option<String>,
    source: Option<String>,
    acknowledged: Option<bool>,
    sort: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CommandsQueryParams {
    page: Option<usize>,
    per_page: Option<usize>,
    command: Option<String>,
    status: Option<String>,
    sort: Option<String>,
}

async fn query_runtime(
    State(registry): State<ApplicationRegistry>,
) -> Json<HttpSuccessEnvelope<RuntimeQueryResult>> {
    Json(success(default_application(&registry).query_runtime()))
}

async fn query_runtime_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<RuntimeQueryResult>>, ApiError> {
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.query_runtime(),
    )))
}

async fn query_orders(
    State(registry): State<ApplicationRegistry>,
    Query(params): Query<OrdersQueryParams>,
) -> Json<HttpSuccessEnvelope<OrdersQueryResult>> {
    let (page, per_page) = normalize_pagination(params.page, params.per_page);
    Json(success(default_application(&registry).query_orders(
        page,
        per_page,
        OrdersFilters {
            side: params.side,
            status: params.status,
        },
    )))
}

async fn query_orders_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Query(params): Query<OrdersQueryParams>,
) -> Result<Json<HttpSuccessEnvelope<OrdersQueryResult>>, ApiError> {
    let (page, per_page) = normalize_pagination(params.page, params.per_page);
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.query_orders(
            page,
            per_page,
            OrdersFilters {
                side: params.side,
                status: params.status,
            },
        ),
    )))
}

async fn query_fills(
    State(registry): State<ApplicationRegistry>,
    Query(params): Query<FillsQueryParams>,
) -> Json<HttpSuccessEnvelope<FillsQueryResult>> {
    let (page, per_page) = normalize_pagination(params.page, params.per_page);
    Json(success(default_application(&registry).query_fills(
        page,
        per_page,
        FillsFilters {
            side: params.side,
            order_id: params.order_id,
            client_order_id: params.client_order_id,
        },
    )))
}

async fn query_fills_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Query(params): Query<FillsQueryParams>,
) -> Result<Json<HttpSuccessEnvelope<FillsQueryResult>>, ApiError> {
    let (page, per_page) = normalize_pagination(params.page, params.per_page);
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.query_fills(
            page,
            per_page,
            FillsFilters {
                side: params.side,
                order_id: params.order_id,
                client_order_id: params.client_order_id,
            },
        ),
    )))
}

async fn query_alerts(
    State(registry): State<ApplicationRegistry>,
    Query(params): Query<AlertsQueryParams>,
) -> Json<HttpSuccessEnvelope<AlertsQueryResult>> {
    let (page, per_page) = normalize_pagination(params.page, params.per_page);
    Json(success(default_application(&registry).query_alerts(
        page,
        per_page,
        AlertsFilters {
            category: params.category,
            severity: params.severity,
            source: params.source,
            acknowledged: params.acknowledged,
        },
        params.sort.as_deref().unwrap_or("created_at_desc"),
    )))
}

async fn query_alerts_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Query(params): Query<AlertsQueryParams>,
) -> Result<Json<HttpSuccessEnvelope<AlertsQueryResult>>, ApiError> {
    let (page, per_page) = normalize_pagination(params.page, params.per_page);
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.query_alerts(
            page,
            per_page,
            AlertsFilters {
                category: params.category,
                severity: params.severity,
                source: params.source,
                acknowledged: params.acknowledged,
            },
            params.sort.as_deref().unwrap_or("created_at_desc"),
        ),
    )))
}

async fn query_commands(
    State(registry): State<ApplicationRegistry>,
    Query(params): Query<CommandsQueryParams>,
) -> Json<HttpSuccessEnvelope<CommandsQueryResult>> {
    let (page, per_page) = normalize_pagination(params.page, params.per_page);
    Json(success(default_application(&registry).query_commands(
        page,
        per_page,
        CommandsFilters {
            command: params.command,
            status: params.status,
        },
        params.sort.as_deref().unwrap_or("requested_at_desc"),
    )))
}

async fn query_commands_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Query(params): Query<CommandsQueryParams>,
) -> Result<Json<HttpSuccessEnvelope<CommandsQueryResult>>, ApiError> {
    let (page, per_page) = normalize_pagination(params.page, params.per_page);
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.query_commands(
            page,
            per_page,
            CommandsFilters {
                command: params.command,
                status: params.status,
            },
            params.sort.as_deref().unwrap_or("requested_at_desc"),
        ),
    )))
}

async fn control_plane_capabilities(
    State(registry): State<ApplicationRegistry>,
) -> Json<HttpSuccessEnvelope<ControlPlaneCapabilities>> {
    Json(success(
        default_application(&registry).control_plane_capabilities(),
    ))
}

async fn control_plane_capabilities_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<ControlPlaneCapabilities>>, ApiError> {
    Ok(Json(success(
        application_for_symbol(&registry, &symbol)?.instance_scoped_control_plane_capabilities(),
    )))
}

async fn pause(
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(default_application(&registry), CommandType::Pause, request).await
}

async fn pause_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(
        application_for_symbol(&registry, &symbol)?,
        CommandType::Pause,
        request,
    )
    .await
}

async fn resume(
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(default_application(&registry), CommandType::Resume, request).await
}

async fn resume_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(
        application_for_symbol(&registry, &symbol)?,
        CommandType::Resume,
        request,
    )
    .await
}

async fn cancel_all(
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(
        default_application(&registry),
        CommandType::CancelAll,
        request,
    )
    .await
}

async fn cancel_all_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(
        application_for_symbol(&registry, &symbol)?,
        CommandType::CancelAll,
        request,
    )
    .await
}

async fn flatten_now(
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(
        default_application(&registry),
        CommandType::FlattenNow,
        request,
    )
    .await
}

async fn flatten_now_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(
        application_for_symbol(&registry, &symbol)?,
        CommandType::FlattenNow,
        request,
    )
    .await
}

async fn shutdown_after_flatten(
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(
        default_application(&registry),
        CommandType::ShutdownAfterFlatten,
        request,
    )
    .await
}

async fn shutdown_after_flatten_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
    Json(request): Json<CommandRequest>,
) -> Result<Json<HttpSuccessEnvelope<CommandAccepted>>, ApiError> {
    issue_command(
        application_for_symbol(&registry, &symbol)?,
        CommandType::ShutdownAfterFlatten,
        request,
    )
    .await
}

async fn emit_price_tick(
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<PriceUpdated>>, ApiError> {
    let tick = default_application(&registry)
        .emit_price_tick()
        .await
        .map_err(|error| ApiError::unavailable(format!("{error:#}")))?;
    Ok(Json(success(tick)))
}

async fn emit_price_tick_for_instance(
    Path(symbol): Path<String>,
    State(registry): State<ApplicationRegistry>,
) -> Result<Json<HttpSuccessEnvelope<PriceUpdated>>, ApiError> {
    let tick = application_for_symbol(&registry, &symbol)?
        .emit_price_tick()
        .await
        .map_err(|error| ApiError::unavailable(format!("{error:#}")))?;
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
        .map_err(|error| ApiError::unavailable(format!("{error:#}")))?;
    Ok(Json(success(accepted)))
}

async fn ws_events(
    ws: WebSocketUpgrade,
    State(registry): State<ApplicationRegistry>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| websocket_session(socket, default_application(&registry)))
}

async fn ws_events_for_instance(
    Path(symbol): Path<String>,
    ws: WebSocketUpgrade,
    State(registry): State<ApplicationRegistry>,
) -> Result<impl IntoResponse, ApiError> {
    let application = application_for_symbol(&registry, &symbol)?;
    Ok(ws.on_upgrade(move |socket| websocket_session(socket, application)))
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

fn normalize_pagination(page: Option<usize>, per_page: Option<usize>) -> (usize, usize) {
    (page.unwrap_or(1).max(1), per_page.unwrap_or(20))
}
