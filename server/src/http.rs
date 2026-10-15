use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use poise_application::exchange::{CandleInterval, MarketCandle};
use poise_application::{
    ConfigurationApplicationError, ConfigurationApplicationRequest, ConfigurationApplicationStatus,
    DiagnosticSeverity, TrackInput, TrackMutationError,
    build_account_analysis_read_model_with_account,
};
use poise_core::track::TrackId;
use poise_engine::command::TrackCommand;
use poise_protocol::{
    AccountSummaryView, ActivityLevelView, BandFlattenTrigger, BandProtectionPolicy,
    BandRecoverPolicy, CandleIntervalView, CandleView, HealthResponse, HealthStatusView,
    HealthTaskStatusView, HealthTaskView, InstrumentView, PnlBackfillStatusView,
    PnlHistoryWindowView, ReadinessIssueView, RecentConfigurationConflictView,
    RecentFillAuditItemView, RecentFillCoverageView, RecentFillsAuditResponse,
    RecentPnlAvailabilityView, RecentPnlCompletenessView, ShapeFamily, SystemInfoResponse,
    TrackChartResponse, TrackCommandAccepted, TrackCommandRequest, TrackCommandType,
    TrackConfigurationApplicationStatusView, TrackConfigurationApplyRequest,
    TrackConfigurationBlockerView, TrackConfigurationCapacityProofView,
    TrackConfigurationFieldErrorView, TrackConfigurationIdentityView, TrackConfigurationImpactView,
    TrackConfigurationPolicyStateView, TrackConfigurationPreviewRequest,
    TrackConfigurationPreviewResponse, TrackConfigurationResponse,
    TrackConfigurationRestartRequiredView, TrackConfigurationRiskOutcomeView,
    TrackConfigurationView, TrackDetailView, TrackDiagnosticItemView, TrackDiagnosticsView,
    TrackListResponse, TrackPositionQuantityUnitView, TradingReadinessResponse,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower_http::services::{ServeDir, ServeFile};

use crate::config_projection::{
    configuration_view_from_definition, project_out_of_band_policy, project_risk_acquisition,
    project_shape_family,
};
use crate::runtime::{
    RuntimeHealthSnapshot, RuntimeHealthStatus, RuntimeTaskHealthSnapshot, assess_trading_readiness,
};
use crate::server_context::{HttpState, WebSocketState};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: String,
}

pub fn router(http_state: HttpState, websocket_state: WebSocketState) -> Router {
    router_with_web_root(
        http_state,
        websocket_state,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../web/dist"),
    )
}

pub(crate) fn router_with_web_root(
    http_state: HttpState,
    websocket_state: WebSocketState,
    web_root: impl Into<PathBuf>,
) -> Router {
    let web_root = web_root.into();
    let index = web_root.join("index.html");

    Router::new()
        .route("/health", get(health))
        .route("/readiness", get(readiness))
        .route("/system", get(get_system))
        .route("/account", get(get_account))
        .route(
            "/tracks",
            get(list_tracks).post(crate::track_creation::save),
        )
        .route("/track-creation", get(crate::track_creation::context))
        .route(
            "/track-creation/preview",
            post(crate::track_creation::preview),
        )
        .route("/tracks/:id", get(get_track_detail))
        .route("/tracks/:id/chart", get(get_track_chart))
        .route("/tracks/:id/configuration", get(get_track_configuration))
        .route("/tracks/:id/configuration", put(apply_track_configuration))
        .route(
            "/tracks/:id/configuration/application/:application_id",
            delete(abandon_track_configuration_application),
        )
        .route(
            "/tracks/:id/configuration/preview",
            post(preview_track_configuration),
        )
        .route("/debug/tracks/:id/diagnostics", get(get_track_diagnostics))
        .route(
            "/debug/tracks/:id/recent-fills-audit",
            get(get_recent_fills_audit),
        )
        .route("/tracks/:id/commands", post(submit_command))
        .route(
            "/ws",
            get(move |ws| crate::websocket::ws_handler(ws, websocket_state.clone())),
        )
        .fallback_service(ServeDir::new(web_root).fallback(ServeFile::new(index)))
        .with_state(http_state)
}

async fn readiness(
    State(state): State<HttpState>,
) -> Result<(StatusCode, Json<TradingReadinessResponse>), (StatusCode, Json<ErrorResponse>)> {
    let tracks = state
        .query_service
        .list_track_sources()
        .await
        .map_err(map_query_error)?;
    let account = state.account_monitor.current_summary().await;
    let stale_exchange_state = state.exchange_freshness.stale_tracks().await;
    let mut response = assess_trading_readiness(
        &tracks,
        account.as_ref(),
        &state.runtime_health.snapshot(),
        &state.pnl_backfill_status.snapshot(),
        &state.effect_journal_diagnostics.snapshot(),
        &state.account_margin_guard.block_snapshot(),
        &stale_exchange_state,
    );
    if let Some(store) = state.configuration_store.as_ref() {
        if let Some(application) = store
            .active_application()
            .map_err(map_configuration_store_error)?
        {
            response.ready_to_trade = false;
            response.blockers.push(ReadinessIssueView {
                component: "configuration_application".to_string(),
                reason: format!(
                    "configuration application `{}` is {}; old orders must reach a confirmed terminal state",
                    application.application_id, application.result
                ),
            });
        }
    }
    let status = if response.ready_to_trade {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    Ok((status, Json(response)))
}

async fn get_system(
    State(state): State<HttpState>,
) -> Result<Json<SystemInfoResponse>, (StatusCode, Json<ErrorResponse>)> {
    let recent_configuration_conflict = state
        .configuration_store
        .as_ref()
        .map(|store| {
            store
                .latest_external_configuration_conflict()
                .map(|conflict| {
                    conflict.map(|conflict| RecentConfigurationConflictView {
                        application_id: conflict.application_id,
                        track_id: conflict.track_id,
                        observed_at: conflict.observed_at.to_rfc3339(),
                    })
                })
        })
        .transpose()
        .map_err(|error| internal_error(error.to_string()))?
        .flatten();
    Ok(Json(SystemInfoResponse {
        exchange: state.system_info.exchange.clone(),
        deployment: state.system_info.deployment.clone(),
        instance_directory: state.system_info.instance_directory.clone(),
        server_version: state.system_info.server_version.clone(),
        bind_address: state.system_info.bind_address.clone(),
        credentials_configured: state.system_info.credentials_configured,
        recent_configuration_conflict,
    }))
}

async fn list_tracks(
    State(state): State<HttpState>,
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
    State(state): State<HttpState>,
) -> Result<(StatusCode, Json<HealthResponse>), (StatusCode, Json<ErrorResponse>)> {
    let sources = state
        .query_service
        .list_track_sources()
        .await
        .map_err(map_query_error)?;
    let attention_required_count = sources
        .iter()
        .filter(|source| {
            source.recovery_issue.is_some()
                || source.has_account_margin_guard
                || source.has_stale_market_data
        })
        .count();
    let runtime_health = state.runtime_health.snapshot();
    let pnl_backfill = state.pnl_backfill_status.snapshot();
    let task_attention_required = runtime_health_has_degraded_task(&runtime_health)
        || pnl_backfill.health_status() == RuntimeHealthStatus::Degraded;
    let status = if attention_required_count == 0 && !task_attention_required {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let mut tasks = project_runtime_health(runtime_health);
    tasks.push(project_pnl_backfill_health(&pnl_backfill));

    Ok((
        status,
        Json(HealthResponse {
            status: if attention_required_count == 0 && !task_attention_required {
                HealthStatusView::Ok
            } else {
                HealthStatusView::AttentionRequired
            },
            track_count: sources.len(),
            attention_required_count,
            tasks,
            pnl_backfill: project_pnl_backfill_status(&pnl_backfill),
        }),
    ))
}

fn runtime_health_has_degraded_task(snapshot: &RuntimeHealthSnapshot) -> bool {
    snapshot
        .tasks
        .iter()
        .any(|task| task.status == RuntimeHealthStatus::Degraded)
}

fn project_runtime_health(snapshot: RuntimeHealthSnapshot) -> Vec<HealthTaskView> {
    snapshot
        .tasks
        .into_iter()
        .map(project_runtime_health_task)
        .collect()
}

fn project_runtime_health_task(task: RuntimeTaskHealthSnapshot) -> HealthTaskView {
    HealthTaskView {
        component: task.component.as_str().to_string(),
        status: match task.status {
            RuntimeHealthStatus::Unknown => HealthTaskStatusView::Unknown,
            RuntimeHealthStatus::Ok => HealthTaskStatusView::Ok,
            RuntimeHealthStatus::Degraded => HealthTaskStatusView::Degraded,
        },
        last_success_at: task.last_success_at.map(|value| value.to_rfc3339()),
        last_error_at: task.last_error_at.map(|value| value.to_rfc3339()),
        last_error: task.last_error,
    }
}

fn project_pnl_backfill_health(snapshot: &crate::runtime::PnlBackfillSnapshot) -> HealthTaskView {
    HealthTaskView {
        component: "pnl_backfill".to_string(),
        status: match snapshot.health_status() {
            RuntimeHealthStatus::Unknown => HealthTaskStatusView::Unknown,
            RuntimeHealthStatus::Ok => HealthTaskStatusView::Ok,
            RuntimeHealthStatus::Degraded => HealthTaskStatusView::Degraded,
        },
        last_success_at: snapshot.last_success_at.map(|value| value.to_rfc3339()),
        last_error_at: snapshot.last_error_at.map(|value| value.to_rfc3339()),
        last_error: snapshot.last_error.clone(),
    }
}

fn project_pnl_backfill_status(
    snapshot: &crate::runtime::PnlBackfillSnapshot,
) -> PnlBackfillStatusView {
    PnlBackfillStatusView {
        last_completed_at: snapshot.last_completed_at.map(|value| value.to_rfc3339()),
        available_tracks: snapshot.available_tracks,
        unsupported_tracks: snapshot.unsupported_tracks,
        incomplete_tracks: snapshot.incomplete_tracks,
        records_seen: snapshot.records_seen,
        records_inserted: snapshot.records_inserted,
        records_skipped: snapshot.records_skipped,
        failures: snapshot.failures,
        last_error_at: snapshot.last_error_at.map(|value| value.to_rfc3339()),
        last_error: snapshot.last_error.clone(),
    }
}

async fn get_account(
    State(state): State<HttpState>,
) -> Result<Json<AccountSummaryView>, (StatusCode, Json<ErrorResponse>)> {
    let summary = state.account_monitor.current_summary().await;
    let analysis = match state.query_service.list_track_sources().await {
        Ok(sources) => Some(build_account_analysis_read_model_with_account(
            summary.as_ref(),
            &sources,
        )),
        Err(error) => {
            tracing::warn!("failed to load account analysis for HTTP account summary: {error}");
            None
        }
    };

    Ok(Json(state.account_projector.project_summary_with_analysis(
        summary.as_ref(),
        analysis.as_ref(),
    )))
}

async fn get_track_detail(
    Path(id): Path<String>,
    State(state): State<HttpState>,
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

#[derive(Debug, Deserialize)]
struct TrackChartQuery {
    interval: Option<CandleIntervalView>,
    limit: Option<usize>,
}

async fn get_track_chart(
    Path(id): Path<String>,
    Query(query): Query<TrackChartQuery>,
    State(state): State<HttpState>,
) -> Result<Json<TrackChartResponse>, (StatusCode, Json<ErrorResponse>)> {
    let track_id = TrackId::new(id.clone());
    let source = state
        .query_service
        .load_track_detail_source(&track_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;
    let interval = query.interval.unwrap_or(CandleIntervalView::FiveMinutes);
    let limit = query.limit.unwrap_or(100);
    if !(20..=100).contains(&limit) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "chart limit must be 20..=100".to_string(),
            }),
        ));
    }
    let candles = state
        .market_data
        .get_candles(
            &source.instrument,
            application_candle_interval(interval),
            limit,
        )
        .await
        .map_err(|error| {
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("failed to load venue candles: {error}"),
                }),
            )
        })?;
    Ok(Json(TrackChartResponse {
        interval,
        candles: candles.into_iter().map(project_candle).collect(),
    }))
}

fn application_candle_interval(interval: CandleIntervalView) -> CandleInterval {
    match interval {
        CandleIntervalView::OneMinute => CandleInterval::OneMinute,
        CandleIntervalView::FiveMinutes => CandleInterval::FiveMinutes,
        CandleIntervalView::FifteenMinutes => CandleInterval::FifteenMinutes,
        CandleIntervalView::OneHour => CandleInterval::OneHour,
    }
}

fn project_candle(candle: MarketCandle) -> CandleView {
    CandleView {
        open_time: candle.open_time.to_rfc3339(),
        open: candle.open,
        high: candle.high,
        low: candle.low,
        close: candle.close,
    }
}

async fn get_track_configuration(
    Path(id): Path<String>,
    State(state): State<HttpState>,
) -> Result<Json<TrackConfigurationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let definitions = state.runtime.committed_track_definitions().await;
    let definition = definitions
        .iter()
        .find(|definition| definition.track_id().as_str() == id)
        .cloned()
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;
    let metadata = state.configuration_metadata.get(&id).ok_or_else(|| {
        internal_error(format!("missing configuration metadata for track `{id}`"))
    })?;
    let configurations = definitions
        .iter()
        .map(|definition| {
            (
                definition.track_id().as_str().to_string(),
                configuration_view_from_definition(definition),
            )
        })
        .collect();
    let version = match state.configuration_store.as_ref() {
        Some(store) => store
            .current_version()
            .map_err(map_configuration_store_error)?,
        None => configuration_version(configurations),
    };
    let application = match state.configuration_store.as_ref() {
        Some(store) => store
            .latest_for_track(&id)
            .map_err(map_configuration_store_error)?,
        None => None,
    };
    let application_status = application
        .as_ref()
        .map(|row| project_application_status(&row.result))
        .unwrap_or(TrackConfigurationApplicationStatusView::Idle);
    Ok(Json(TrackConfigurationResponse {
        version,
        identity: TrackConfigurationIdentityView {
            id: definition.track_id().as_str().to_string(),
            instrument: InstrumentView {
                venue: definition.instrument().venue.as_str().to_string(),
                symbol: definition.instrument().symbol.clone(),
            },
        },
        active: configuration_view_from_definition(&definition),
        restart_required: TrackConfigurationRestartRequiredView {
            leverage: metadata.leverage,
            tick_timeout_secs: metadata.tick_timeout_secs,
        },
        application_status,
        application: application.map(project_application_view),
    }))
}

async fn abandon_track_configuration_application(
    Path((id, application_id)): Path<(String, String)>,
    State(state): State<HttpState>,
) -> Result<
    Json<poise_protocol::TrackConfigurationApplicationView>,
    (StatusCode, Json<ErrorResponse>),
> {
    let store = state.configuration_store.as_ref().ok_or_else(|| {
        internal_error("configuration application store is unavailable".to_string())
    })?;
    let receipt = state
        .runtime
        .abandon_configuration_application(
            &TrackId::new(id.clone()),
            &application_id,
            store.clone(),
        )
        .await
        .map_err(map_configuration_application_error)?;
    Ok(Json(poise_protocol::TrackConfigurationApplicationView {
        application_id,
        status: project_runtime_application_status(receipt.status),
        accepted: false,
        retryable: false,
        message: Some(
            "configuration application was abandoned; old orders are still being reconciled before the previous definition can resume".to_string(),
        ),
        active_version: None,
    }))
}

async fn apply_track_configuration(
    Path(id): Path<String>,
    State(state): State<HttpState>,
    Json(request): Json<TrackConfigurationApplyRequest>,
) -> Result<
    Json<poise_protocol::TrackConfigurationApplicationView>,
    (StatusCode, Json<ErrorResponse>),
> {
    let store = state.configuration_store.as_ref().ok_or_else(|| {
        internal_error("configuration application store is unavailable".to_string())
    })?;
    let request_hash =
        configuration_request_hash(&id, &request.expected_version, &request.candidate);
    let existing = store
        .application_by_id(&request.application_id)
        .map_err(map_configuration_store_error)?;
    if let Some(existing) = existing.as_ref() {
        if existing.track_id != id || existing.request_hash != request_hash {
            return Err(map_configuration_store_error(
                poise_application::ConfigurationApplicationStoreError::ApplicationIdConflict,
            ));
        }
        if matches!(
            existing.result,
            poise_storage::sqlite::ConfigurationApplicationAuditStatus::Active
                | poise_storage::sqlite::ConfigurationApplicationAuditStatus::WaitingForOrders
        ) {
            return Ok(Json(project_application_view(existing.clone())));
        }
    }
    if let Some(active) = store
        .active_application()
        .map_err(map_configuration_store_error)?
        && active.application_id != request.application_id
    {
        return Err(map_configuration_store_error(
            poise_application::ConfigurationApplicationStoreError::ApplicationIdConflict,
        ));
    }
    let source = state
        .query_service
        .load_track_detail_source(&TrackId::new(id.clone()))
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;
    let metadata = state.configuration_metadata.get(&id).ok_or_else(|| {
        internal_error(format!("missing configuration metadata for track `{id}`"))
    })?;
    let candidate =
        configuration_definition(&source, &request.candidate, metadata.tick_timeout_secs)
            .map_err(|message| bad_request(message))?;
    if let Some(existing) = existing.filter(|existing| {
        matches!(
            existing.result,
            poise_storage::sqlite::ConfigurationApplicationAuditStatus::Preparing
                | poise_storage::sqlite::ConfigurationApplicationAuditStatus::Activating
        )
    }) {
        // A persisted preparing/activating application is retried without
        // consulting volatile price/capacity facts. The runtime determines
        // whether the definition was already installed and completes its audit.
        let application_request = ConfigurationApplicationRequest {
            application_id: request.application_id.clone(),
            request_hash,
            expected_version: request.expected_version,
            candidate_version: existing.candidate_version.clone(),
            candidate,
        };
        let receipt = state
            .runtime
            .apply_configuration_application(&TrackId::new(id), application_request, store.clone())
            .await
            .map_err(map_configuration_application_error)?;
        return Ok(Json(poise_protocol::TrackConfigurationApplicationView {
            application_id: request.application_id,
            status: project_runtime_application_status(receipt.status),
            accepted: true,
            retryable: false,
            message: None,
            active_version: (receipt.status == ConfigurationApplicationStatus::Active)
                .then_some(existing.candidate_version),
        }));
    }
    // PUT repeats the preview's fail-closed evaluation using the current facts.
    let Json(preview) = preview_track_configuration(
        Path(id.clone()),
        State(state.clone()),
        Json(TrackConfigurationPreviewRequest {
            candidate: request.candidate.clone(),
        }),
    )
    .await?;
    if !preview.valid {
        return Err(bad_request(
            "configuration candidate no longer passes current validation".to_string(),
        ));
    }
    let candidate_version = store
        .candidate_version(&id, &request.candidate)
        .map_err(map_configuration_store_error)?;
    let application_request = ConfigurationApplicationRequest {
        application_id: request.application_id.clone(),
        request_hash,
        expected_version: request.expected_version,
        candidate_version,
        candidate,
    };
    let active_version = application_request.candidate_version.clone();
    let receipt = state
        .runtime
        .apply_configuration_application(
            &TrackId::new(id.clone()),
            application_request,
            store.clone(),
        )
        .await
        .map_err(map_configuration_application_error)?;
    Ok(Json(poise_protocol::TrackConfigurationApplicationView {
        application_id: request.application_id,
        status: project_runtime_application_status(receipt.status),
        accepted: true,
        retryable: false,
        message: None,
        active_version: (receipt.status == ConfigurationApplicationStatus::Active)
            .then_some(active_version),
    }))
}

fn configuration_request_hash(
    id: &str,
    expected_version: &str,
    candidate: &TrackConfigurationView,
) -> String {
    let payload = serde_json::to_vec(&(id, expected_version, candidate))
        .expect("configuration request serializes");
    format!("{:x}", Sha256::digest(payload))
}

fn project_application_status(
    value: &poise_storage::sqlite::ConfigurationApplicationAuditStatus,
) -> TrackConfigurationApplicationStatusView {
    match value {
        poise_storage::sqlite::ConfigurationApplicationAuditStatus::Preparing => {
            TrackConfigurationApplicationStatusView::Preparing
        }
        poise_storage::sqlite::ConfigurationApplicationAuditStatus::WaitingForOrders => {
            TrackConfigurationApplicationStatusView::WaitingForOrders
        }
        poise_storage::sqlite::ConfigurationApplicationAuditStatus::Activating => {
            TrackConfigurationApplicationStatusView::Activating
        }
        poise_storage::sqlite::ConfigurationApplicationAuditStatus::Active => {
            TrackConfigurationApplicationStatusView::Active
        }
        poise_storage::sqlite::ConfigurationApplicationAuditStatus::Failed
        | poise_storage::sqlite::ConfigurationApplicationAuditStatus::Interrupted => {
            TrackConfigurationApplicationStatusView::Failed
        }
    }
}

fn project_runtime_application_status(
    value: ConfigurationApplicationStatus,
) -> TrackConfigurationApplicationStatusView {
    match value {
        ConfigurationApplicationStatus::Idle => TrackConfigurationApplicationStatusView::Idle,
        ConfigurationApplicationStatus::Preparing => {
            TrackConfigurationApplicationStatusView::Preparing
        }
        ConfigurationApplicationStatus::WaitingForOrders => {
            TrackConfigurationApplicationStatusView::WaitingForOrders
        }
        ConfigurationApplicationStatus::Activating => {
            TrackConfigurationApplicationStatusView::Activating
        }
        ConfigurationApplicationStatus::Active => TrackConfigurationApplicationStatusView::Active,
        ConfigurationApplicationStatus::Failed => TrackConfigurationApplicationStatusView::Failed,
    }
}

fn project_application_view(
    row: poise_storage::sqlite::ConfigurationApplicationAuditRow,
) -> poise_protocol::TrackConfigurationApplicationView {
    let message = match row.result {
        poise_storage::sqlite::ConfigurationApplicationAuditStatus::Interrupted => Some(
            "configuration application was interrupted before the candidate configuration was persisted; retry with the same application ID".to_string(),
        ),
        poise_storage::sqlite::ConfigurationApplicationAuditStatus::Failed => {
            Some("configuration application failed".to_string())
        }
        poise_storage::sqlite::ConfigurationApplicationAuditStatus::WaitingForOrders => Some(
            "configuration application is waiting for old orders to reach a confirmed terminal state".to_string(),
        ),
        _ => None,
    };
    poise_protocol::TrackConfigurationApplicationView {
        application_id: row.application_id,
        status: project_application_status(&row.result),
        accepted: !matches!(
            row.result,
            poise_storage::sqlite::ConfigurationApplicationAuditStatus::Failed
                | poise_storage::sqlite::ConfigurationApplicationAuditStatus::Interrupted
        ),
        retryable: row.result
            == poise_storage::sqlite::ConfigurationApplicationAuditStatus::Interrupted,
        message,
        active_version: (row.result
            == poise_storage::sqlite::ConfigurationApplicationAuditStatus::Active)
            .then_some(row.candidate_version),
    }
}

pub(crate) fn map_configuration_store_error(
    error: poise_application::ConfigurationApplicationStoreError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        poise_application::ConfigurationApplicationStoreError::VersionConflict
        | poise_application::ConfigurationApplicationStoreError::ExternalChange
        | poise_application::ConfigurationApplicationStoreError::ApplicationIdConflict => (
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: error.to_string(),
            }),
        ),
        _ => internal_error(error.to_string()),
    }
}
fn map_configuration_application_error(
    error: ConfigurationApplicationError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        ConfigurationApplicationError::Terminated
        | ConfigurationApplicationError::RecoveryAnomaly
        | ConfigurationApplicationError::PersistedApplicationCannotBeAbandoned
        | ConfigurationApplicationError::ApplicationIdMismatch
        | ConfigurationApplicationError::ApplicationInProgress => (
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: error.to_string(),
            }),
        ),
        ConfigurationApplicationError::Persistence(error) => map_configuration_store_error(error),
        _ => internal_error(error.to_string()),
    }
}

fn configuration_view(source: &poise_application::TrackReadModel) -> TrackConfigurationView {
    TrackConfigurationView {
        lower_price: source.lower_price,
        upper_price: source.upper_price,
        long_exposure_units: source.long_exposure_units,
        short_exposure_units: source.short_exposure_units,
        notional_per_unit: source.notional_per_unit,
        min_rebalance_units: source.min_rebalance_units,
        shape_family: project_shape_family(source.shape_family),
        out_of_band_policy: project_out_of_band_policy(source.out_of_band_policy),
        max_notional: source.max_notional,
        daily_loss_limit: source.loss_limits.daily_loss_limit,
        total_loss_limit: source.loss_limits.total_loss_limit,
        risk_acquisition: project_risk_acquisition(source.risk_acquisition_config),
    }
}

fn configuration_version(mut configurations: Vec<(String, TrackConfigurationView)>) -> String {
    configurations.sort_by(|left, right| left.0.cmp(&right.0));
    let payload = serde_json::to_vec(&configurations).expect("configuration projection serializes");
    format!("{:x}", Sha256::digest(payload))
}

async fn preview_track_configuration(
    Path(id): Path<String>,
    State(state): State<HttpState>,
    Json(request): Json<TrackConfigurationPreviewRequest>,
) -> Result<Json<TrackConfigurationPreviewResponse>, (StatusCode, Json<ErrorResponse>)> {
    let source = state
        .query_service
        .load_track_detail_source(&TrackId::new(id.clone()))
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;
    let metadata = state.configuration_metadata.get(&id).ok_or_else(|| {
        internal_error(format!("missing configuration metadata for track `{id}`"))
    })?;
    let candidate =
        match configuration_definition(&source, &request.candidate, metadata.tick_timeout_secs) {
            Ok(value) => value,
            Err(message) => {
                let field = configuration_error_field(&message);
                return Ok(Json(TrackConfigurationPreviewResponse {
                    previewed_at: chrono::Utc::now().to_rfc3339(),
                    valid: false,
                    field_errors: vec![TrackConfigurationFieldErrorView {
                        field: field.to_string(),
                        message,
                    }],
                    blockers: vec![],
                    impact: None,
                    capacity_proof: None,
                }));
            }
        };
    let active = configuration_definition(
        &source,
        &configuration_view(&source),
        metadata.tick_timeout_secs,
    )
    .map_err(internal_error)?;
    let strategy_price = match (source.strategy_price_status, source.strategy_price) {
        (poise_application::TrackStrategyPriceStatus::Live, Some(price))
            if price.is_finite() && price > 0.0 =>
        {
            price
        }
        _ => {
            return Ok(Json(preview_blocked(
                "market_data_unavailable",
                "live strategy price is unavailable",
            )));
        }
    };
    let account = match state.account_monitor.current_summary().await {
        Some(value) => value,
        None => {
            return Ok(Json(preview_blocked(
                "capacity_proof_unavailable",
                "account capacity proof is unavailable",
            )));
        }
    };
    let account_snapshot = poise_application::exchange::AccountSummarySnapshot::try_new(
        poise_core::instrument::Amount::new(account.equity, account.valuation_asset.clone())
            .map_err(internal_error)?,
        account
            .available
            .map(|value| {
                poise_core::instrument::Amount::new(value, account.valuation_asset.clone())
            })
            .transpose()
            .map_err(internal_error)?,
        account.available_by_asset.clone(),
        account
            .unrealized_pnl
            .map(|value| {
                poise_core::instrument::Amount::new(value, account.valuation_asset.clone())
            })
            .transpose()
            .map_err(internal_error)?,
        account.updated_at,
    )
    .map_err(internal_error)?;
    let capacity_mark_price = match source.instrument_spec.position_basis() {
        poise_core::instrument::PositionBasis::BaseAsset => None,
        poise_core::instrument::PositionBasis::Contracts { .. } => match source
            .mark_price
            .filter(|price| price.is_finite() && *price > 0.0)
        {
            Some(price) => Some(price),
            None => {
                return Ok(Json(preview_blocked(
                    "market_data_unavailable",
                    "live mark price is unavailable for inverse capacity proof",
                )));
            }
        },
    };
    let capacity = match crate::account_capacity::estimate_account_capacity(
        &account_snapshot,
        &source.instrument_spec,
        capacity_mark_price,
        metadata.leverage,
    ) {
        Ok(value) => value,
        Err(error) => {
            return Ok(Json(preview_blocked(
                "capacity_proof_unavailable",
                &error.to_string(),
            )));
        }
    };
    let cumulative = match source.pnl.net_realized_pnl_cumulative.complete_value() {
        Some(value) => value,
        None => {
            return Ok(Json(preview_blocked(
                "risk_proof_unavailable",
                "cumulative realized PnL is incomplete",
            )));
        }
    };
    if source.pnl.asset != *source.instrument_spec.settlement_asset() {
        return Ok(Json(preview_blocked(
            "risk_proof_unavailable",
            "PnL asset does not match settlement asset",
        )));
    }
    let output = poise_engine::configuration_preview::preview_configuration(
        poise_engine::configuration_preview::ConfigurationPreviewInput {
            active,
            candidate,
            instrument_spec: source.instrument_spec.clone(),
            current_position_qty: poise_core::instrument::PositionQty::new(source.position_qty)
                .map_err(internal_error)?,
            loss_guard: poise_core::risk::LossGuardSnapshot {
                net_realized_pnl_today: source.pnl.gross_realized_pnl_today
                    - source.pnl.trading_fee_today
                    + source.pnl.funding_fee_today,
                net_realized_pnl_cumulative: cumulative,
                unrealized_pnl: source.pnl.unrealized_pnl,
            },
            strategy_price,
            active_policy_target: source
                .decision
                .policy_target
                .map(poise_core::types::Exposure),
            observed_at: chrono::Utc::now(),
            max_increase_notional: capacity.max_increase_notional,
        },
    )
    .map_err(|error| {
        internal_error(format!(
            "configuration preview rejected committed facts: {error:?}"
        ))
    })?;
    let blockers = output
        .blockers
        .iter()
        .map(project_preview_blocker)
        .collect::<Vec<_>>();
    let proof = TrackConfigurationCapacityProofView {
        available_increase_notional: output.available_increase_notional,
        required_increase_notional: output.required_increase_notional,
        asset: capacity.available.asset().as_str().to_string(),
        observed_at: account.updated_at.to_rfc3339(),
    };
    let position_quantity_unit = match source.instrument_spec.position_basis() {
        poise_core::instrument::PositionBasis::BaseAsset => {
            TrackPositionQuantityUnitView::BaseAsset
        }
        poise_core::instrument::PositionBasis::Contracts { .. } => {
            TrackPositionQuantityUnitView::Contracts
        }
    };
    Ok(Json(TrackConfigurationPreviewResponse {
        previewed_at: chrono::Utc::now().to_rfc3339(),
        valid: blockers.is_empty(),
        field_errors: vec![],
        blockers,
        impact: Some(TrackConfigurationImpactView {
            strategy_price,
            active_policy_target: output.active_policy_target.map(|value| value.0),
            candidate_curve_target: output.candidate_curve_target.0,
            candidate_policy_target: output.candidate_policy_target.0,
            candidate_policy_state: project_policy_state(output.candidate_policy_state),
            candidate_risk_approved_target: output.candidate_risk_approved_target.0,
            candidate_risk_outcome: project_risk_outcome(output.candidate_risk_outcome),
            position_quantity: source.position_qty,
            position_quantity_unit,
            current_exposure: output.active_current_exposure.0,
            candidate_current_exposure: output.candidate_current_exposure.0,
            active_policy_target_position_qty: output
                .active_policy_target_position_qty
                .map(|value| value.value()),
            candidate_policy_target_position_qty: output
                .candidate_policy_target_position_qty
                .value(),
            target_position_qty_delta: output.target_position_qty_delta.map(|value| value.value()),
            active_policy_target_signed_notional: output
                .active_policy_target_signed_notional
                .map(|value| value.value()),
            candidate_policy_target_signed_notional: output
                .candidate_policy_target_signed_notional
                .value(),
            target_signed_notional_delta: output
                .target_signed_notional_delta
                .map(|value| value.value()),
            candidate_target_delta_from_current_exposure: output
                .candidate_target_delta_from_current_exposure
                .0,
            candidate_position_qty_delta_from_current: output
                .candidate_position_qty_delta_from_current
                .value(),
            initial_frontier: output.initial_frontier.0,
            initial_execution_target: output.initial_execution_target.0,
            position_qty_per_unit: output.candidate_position_qty_per_unit,
            minimum_rebalance_position_qty: output.minimum_rebalance_raw_quantity.value(),
            normalized_minimum_rebalance_position_qty: output.minimum_rebalance_quantity.value(),
            minimum_rebalance_notional: output.minimum_rebalance_notional.value(),
            maximum_target_position_qty: output.maximum_target_position_qty.value(),
            notional_asset: source.instrument_spec.notional_asset().as_str().to_string(),
        }),
        capacity_proof: Some(proof),
    }))
}

fn configuration_error_field(message: &str) -> &'static str {
    if message.contains("max_release_steps") {
        return "risk_acquisition.max_release_steps";
    }
    [
        "lower_price",
        "upper_price",
        "long_exposure_units",
        "short_exposure_units",
        "notional_per_unit",
        "min_rebalance_units",
        "max_notional",
        "daily_loss_limit",
        "total_loss_limit",
        "initial_ratio",
        "advantage_steps",
        "min_release_steps",
        "max_release_steps",
        "catchup_ratio",
        "stale_release_minutes",
    ]
    .into_iter()
    .find(|field| message.contains(field))
    .map(|field| {
        if field.ends_with("ratio") || field.ends_with("steps") || field == "stale_release_minutes"
        {
            match field {
                "initial_ratio" => "risk_acquisition.initial_ratio",
                "advantage_steps" => "risk_acquisition.advantage_steps",
                "min_release_steps" => "risk_acquisition.min_release_steps",
                "max_release_steps" => "risk_acquisition.max_release_steps",
                "catchup_ratio" => "risk_acquisition.catchup_ratio",
                _ => "risk_acquisition.stale_release_minutes",
            }
        } else {
            field
        }
    })
    .unwrap_or("candidate")
}

fn project_preview_blocker(
    blocker: &poise_engine::configuration_preview::ConfigurationPreviewBlocker,
) -> TrackConfigurationBlockerView {
    match blocker {
        poise_engine::configuration_preview::ConfigurationPreviewBlocker::InstrumentMismatch => TrackConfigurationBlockerView { code: "instrument_mismatch".to_string(), message: "candidate instrument does not match the committed track".to_string() },
        poise_engine::configuration_preview::ConfigurationPreviewBlocker::VenueMinimum => TrackConfigurationBlockerView { code: "venue_minimum".to_string(), message: "candidate target cannot satisfy the venue minimum".to_string() },
        poise_engine::configuration_preview::ConfigurationPreviewBlocker::InsufficientIncreaseNotional => TrackConfigurationBlockerView { code: "insufficient_increase_notional".to_string(), message: "available account capacity cannot cover the required increase".to_string() },
    }
}

fn project_policy_state(
    value: poise_engine::configuration_preview::ConfigurationPolicyState,
) -> TrackConfigurationPolicyStateView {
    use poise_engine::configuration_preview::ConfigurationPolicyState as State;
    match value {
        State::InBand => TrackConfigurationPolicyStateView::InBand,
        State::Frozen => TrackConfigurationPolicyStateView::Frozen,
        State::Flattening => TrackConfigurationPolicyStateView::Flattening,
        State::FlattenPending => TrackConfigurationPolicyStateView::FlattenPending,
        State::Terminated => TrackConfigurationPolicyStateView::Terminated,
    }
}

fn project_risk_outcome(
    value: poise_engine::configuration_preview::ConfigurationRiskOutcome,
) -> TrackConfigurationRiskOutcomeView {
    use poise_engine::configuration_preview::ConfigurationRiskOutcome as Outcome;
    match value {
        Outcome::Allowed => TrackConfigurationRiskOutcomeView::Allowed,
        Outcome::Capped => TrackConfigurationRiskOutcomeView::Capped,
        Outcome::DailyLossTerminated => TrackConfigurationRiskOutcomeView::DailyLossTerminated,
        Outcome::TotalLossTerminated => TrackConfigurationRiskOutcomeView::TotalLossTerminated,
    }
}

fn preview_blocked(code: &str, message: &str) -> TrackConfigurationPreviewResponse {
    TrackConfigurationPreviewResponse {
        previewed_at: chrono::Utc::now().to_rfc3339(),
        valid: false,
        field_errors: vec![],
        blockers: vec![TrackConfigurationBlockerView {
            code: code.to_string(),
            message: message.to_string(),
        }],
        impact: None,
        capacity_proof: None,
    }
}

fn configuration_definition(
    source: &poise_application::TrackReadModel,
    view: &TrackConfigurationView,
    tick_timeout_secs: u64,
) -> Result<poise_core::track::TrackDefinition, String> {
    use poise_core::strategy::{
        BandFlattenTrigger as CoreTrigger, BandProtectionPolicy as CorePolicy,
        BandRecoverPolicy as CoreRecover, RiskAcquisitionConfig, ShapeFamily as CoreShape,
        TrackConfig,
    };
    let shape_family = match view.shape_family {
        ShapeFamily::Linear => CoreShape::Linear,
        ShapeFamily::Inertial => CoreShape::Inertial,
        ShapeFamily::Responsive => CoreShape::Responsive,
    };
    let out_of_band_policy = match &view.out_of_band_policy {
        BandProtectionPolicy::Freeze => CorePolicy::Freeze,
        BandProtectionPolicy::Terminate => CorePolicy::Terminate,
        BandProtectionPolicy::Flatten { trigger, recover } => CorePolicy::Flatten {
            trigger: match trigger {
                BandFlattenTrigger::Immediate => CoreTrigger::Immediate,
                BandFlattenTrigger::FlattenConfirm { bps } => {
                    CoreTrigger::FlattenConfirm { bps: *bps }
                }
            },
            recover: match recover {
                BandRecoverPolicy::BackInBand => CoreRecover::BackInBand,
                BandRecoverPolicy::ReentryConfirm { bps } => {
                    CoreRecover::ReentryConfirm { bps: *bps }
                }
            },
        },
    };
    poise_core::track::TrackDefinition::try_new(
        poise_core::track::TrackId::new(source.track_id.clone()),
        source.instrument.clone(),
        TrackConfig {
            lower_price: view.lower_price,
            upper_price: view.upper_price,
            long_exposure_units: view.long_exposure_units,
            short_exposure_units: view.short_exposure_units,
            notional_per_unit: view.notional_per_unit,
            min_rebalance_units: view.min_rebalance_units,
            shape_family,
            out_of_band_policy,
            risk_acquisition: RiskAcquisitionConfig {
                initial_ratio: view.risk_acquisition.initial_ratio,
                advantage_steps: view.risk_acquisition.advantage_steps,
                min_release_steps: view.risk_acquisition.min_release_steps,
                max_release_steps: view.risk_acquisition.max_release_steps,
                catchup_ratio: view.risk_acquisition.catchup_ratio,
                stale_release_minutes: view.risk_acquisition.stale_release_minutes,
            },
        },
        Some(view.max_notional),
        poise_core::risk::LossLimits {
            daily_loss_limit: view.daily_loss_limit,
            total_loss_limit: view.total_loss_limit,
        },
        Some(tick_timeout_secs),
    )
}

async fn get_track_diagnostics(
    Path(id): Path<String>,
    State(state): State<HttpState>,
) -> Result<Json<TrackDiagnosticsView>, (StatusCode, Json<ErrorResponse>)> {
    let track_id = TrackId::new(id.clone());
    let diagnostics = state
        .debug_query_service
        .load_track_diagnostics(&track_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;

    Ok(Json(TrackDiagnosticsView {
        items: diagnostics
            .into_iter()
            .map(|item| TrackDiagnosticItemView {
                ts: item.observed_at.to_rfc3339(),
                message: item.message,
                level: project_diagnostic_severity(item.severity),
            })
            .collect(),
    }))
}

async fn get_recent_fills_audit(
    Path(id): Path<String>,
    State(state): State<HttpState>,
) -> Result<Json<RecentFillsAuditResponse>, (StatusCode, Json<ErrorResponse>)> {
    let track_id = TrackId::new(id.clone());
    let source = state
        .query_service
        .load_track_detail_source(&track_id)
        .await
        .map_err(map_query_error)?
        .ok_or_else(|| not_found(format!("track `{id}` not found")))?;
    let audit = state
        .recent_fills_auditor
        .audit_recent_fills(&track_id, &source.instrument)
        .await
        .map_err(map_recent_fills_audit_error)?;

    Ok(Json(project_recent_fills_audit(
        source.track_id,
        source.instrument,
        audit,
    )))
}

fn project_recent_fills_audit(
    track_id: String,
    instrument: poise_core::track::Instrument,
    audit: crate::pnl_audit::RecentFillsAudit,
) -> RecentFillsAuditResponse {
    RecentFillsAuditResponse {
        track_id,
        instrument: InstrumentView {
            venue: instrument.venue.as_str().to_string(),
            symbol: instrument.symbol,
        },
        availability: match audit.availability {
            crate::pnl_audit::RecentPnlAvailability::Available => {
                RecentPnlAvailabilityView::Available
            }
            crate::pnl_audit::RecentPnlAvailability::Unsupported => {
                RecentPnlAvailabilityView::Unsupported
            }
        },
        required_window: PnlHistoryWindowView {
            start: audit.required_window.start.to_rfc3339(),
            end: audit.required_window.end.to_rfc3339(),
        },
        covered_window: audit.covered_window.map(|window| PnlHistoryWindowView {
            start: window.start.to_rfc3339(),
            end: window.end.to_rfc3339(),
        }),
        completeness: match audit.completeness {
            poise_application::exchange::RecentPnlCompleteness::Complete => {
                RecentPnlCompletenessView::Complete
            }
            poise_application::exchange::RecentPnlCompleteness::Incomplete => {
                RecentPnlCompletenessView::Incomplete
            }
        },
        records_seen: audit.records_seen,
        records_recorded: audit.records_recorded,
        records_missing: audit.records_missing,
        records_unkeyed: audit.records_unkeyed,
        items: audit
            .items
            .into_iter()
            .map(|item| RecentFillAuditItemView {
                source_key: item.record.source_key,
                trade_id: item.record.trade_id,
                occurred_at: item.record.occurred_at.to_rfc3339(),
                coverage: match item.coverage {
                    crate::pnl_audit::RecentFillCoverage::Recorded => {
                        RecentFillCoverageView::Recorded
                    }
                    crate::pnl_audit::RecentFillCoverage::Missing => {
                        RecentFillCoverageView::Missing
                    }
                    crate::pnl_audit::RecentFillCoverage::Unkeyed => {
                        RecentFillCoverageView::Unkeyed
                    }
                },
            })
            .collect(),
    }
}

async fn submit_command(
    Path(id): Path<String>,
    State(state): State<HttpState>,
    Json(request): Json<TrackCommandRequest>,
) -> Result<Json<TrackCommandAccepted>, (StatusCode, Json<ErrorResponse>)> {
    if !state.runtime.has_track(&id).await {
        return Err(not_found(format!("track `{id}` not found")));
    }

    let command = map_command(request.command)?;
    state
        .runtime
        .handle(&TrackId::new(&id), TrackInput::Command(command))
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

pub(crate) fn bad_request(message: String) -> (StatusCode, Json<ErrorResponse>) {
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

pub(crate) fn internal_error(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse { error: message }),
    )
}

fn service_unavailable(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse { error: message }),
    )
}

fn map_recent_fills_audit_error(
    error: crate::pnl_audit::RecentFillsAuditError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        error @ crate::pnl_audit::RecentFillsAuditError::Exchange(_) => {
            service_unavailable(error.to_string())
        }
        error @ crate::pnl_audit::RecentFillsAuditError::Store(_) => {
            internal_error(error.to_string())
        }
    }
}

fn map_query_error(error: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    internal_error(error.to_string())
}

fn project_diagnostic_severity(severity: DiagnosticSeverity) -> ActivityLevelView {
    match severity {
        DiagnosticSeverity::Info => ActivityLevelView::Info,
        DiagnosticSeverity::Warn => ActivityLevelView::Warn,
    }
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
    use std::fs;
    use std::sync::Arc;

    use anyhow::anyhow;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use chrono::{TimeZone, Utc};
    use poise_application::exchange::{
        AccountPort, AccountSummarySnapshot, CandleInterval, MarketCandle, MarketDataPort,
        MarketDataTick, RecentTrackPnlRecords, UserDataEvent,
    };
    use poise_application::{
        CommittedTrackWrite, EffectJournalEntry, EffectStatusUpdate, PersistedTrackEffect,
        StoredTrackEvent, TrackEffectJournal, TrackIdentityStore, TrackMutationStore,
        TrackQueryStore,
    };
    use poise_core::instrument::{Amount, Asset, InstrumentSpec};
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::{
        events::DomainEvent,
        types::{Exposure, Side},
    };
    use poise_engine::clock::ClockPort;
    use poise_engine::ledger::TrackPnlRecord;
    use poise_engine::manager::TrackManager;
    use poise_protocol::{
        AccountSummaryView, DecisionReasonView, ExecutionBindingIntentView,
        ExecutionBindingStatusView, ExecutionStatusView, RecentFillCoverageView,
        RecentFillsAuditResponse, RecentPnlAvailabilityView, RecentPnlCompletenessView,
        RiskSignalView, TrackChartResponse, TrackCommandAccepted, TrackCommandRequest,
        TrackCommandType, TrackConfigurationView, TrackDetailView, TrackDiagnosticsView,
        TrackListResponse, TrackStatus, TradingReadinessResponse,
    };
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    use crate::account_projector::AccountProjector;
    use crate::pnl_audit::RecentFillsAuditor;
    use crate::projector::TrackProjector;
    use crate::runtime::{PnlBackfillStatus, RuntimeHealth, RuntimeHealthComponent};
    use crate::server_context::{HttpState, WebSocketState};
    use crate::test_support::{
        build_http_state, build_test_application_services, build_websocket_state,
        unavailable_account_monitor,
    };

    use poise_application::{
        AccountMonitor, AccountMonitorConfig, AccountMonitorStore, ApplicationNotification,
        StoredAccountMonitorState, TrackDebugQueryService, TradingQueries,
    };

    fn amount(value: f64, asset: &str) -> Amount {
        Amount::new(value, Asset::try_new(asset).unwrap()).unwrap()
    }

    #[derive(Clone)]
    struct HttpTestState {
        http_state: HttpState,
        websocket_state: WebSocketState,
        _configuration_dir: Option<Arc<tempfile::TempDir>>,
    }

    struct ChartMarketData;

    #[async_trait::async_trait]
    impl MarketDataPort for ChartMarketData {
        async fn subscribe_prices(
            &self,
            _instrument: &Instrument,
        ) -> anyhow::Result<mpsc::Receiver<MarketDataTick>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }

        async fn get_mark_price(&self, _instrument: &Instrument) -> anyhow::Result<f64> {
            Ok(102.0)
        }

        async fn get_candles(
            &self,
            instrument: &Instrument,
            interval: CandleInterval,
            limit: usize,
        ) -> anyhow::Result<Vec<MarketCandle>> {
            assert_eq!(instrument.symbol, "BTCUSDT");
            assert_eq!(interval, CandleInterval::FiveMinutes);
            assert_eq!(limit, 20);
            Ok(vec![
                MarketCandle::try_new(
                    Utc.with_ymd_and_hms(2026, 9, 9, 0, 0, 0).unwrap(),
                    100.0,
                    103.0,
                    99.0,
                    102.0,
                )
                .unwrap(),
            ])
        }
    }

    fn configuration_file() -> String {
        "bind_address = '127.0.0.1:0'\n[exchange]\nvenue = 'binance'\napi_key = 'test-key'\napi_secret = 'test-secret'\n[[tracks]]\ntrack_id = 'btc-core'\nsymbol = 'BTCUSDT'\nlower_price = 90.0\nupper_price = 110.0\nlong_exposure_units = 8.0\nshort_exposure_units = 8.0\nnotional_per_unit = 375.0\nmin_rebalance_units = 0.5\nshape_family = 'linear'\nout_of_band_policy = 'freeze'\nmax_notional = 3000.0\nleverage = 10\ndaily_loss_limit = 100.0\ntotal_loss_limit = 300.0\n".to_string()
    }

    fn with_configuration_store(
        mut state: HttpTestState,
    ) -> (
        HttpTestState,
        Arc<SqliteStorage>,
        std::path::PathBuf,
        Arc<tempfile::TempDir>,
    ) {
        let directory = Arc::new(tempfile::tempdir().unwrap());
        let path = directory.path().join("config.toml");
        std::fs::write(&path, configuration_file()).unwrap();
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        state.http_state.configuration_store = Some(Arc::new(
            crate::config_persistence::ConfigurationApplicationStoreAdapter::new(
                path.clone(),
                storage.clone(),
            )
            .unwrap(),
        ));
        state._configuration_dir = Some(directory.clone());
        (state, storage, path, directory)
    }

    fn router(state: HttpTestState) -> axum::Router {
        super::router(state.http_state, state.websocket_state)
    }

    #[tokio::test]
    async fn creation_http_saves_pending_track_without_activation_and_preserves_online_edits() {
        let (mut state, audit, path, _directory) =
            with_configuration_store(app_state_with_usdt_account_summary().await);
        let (_fixture_dir, _store, ports, candidate, _exchange) =
            crate::track_creation::tests::fixture();
        state.http_state.track_creation_exchange = Some(ports);
        let app = router(state.clone());
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/track-creation")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        let context: poise_protocol::TrackCreationContext =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert!(context.pending.is_empty());
        let request = poise_protocol::TrackCreationPreviewRequest {
            expected_version: context.version.clone(),
            candidate: candidate.clone(),
        };
        let preview = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/track-creation/preview")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = preview.status();
        let body = to_bytes(preview.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let preview: poise_protocol::TrackCreationPreviewResponse =
            serde_json::from_slice(&body).unwrap();
        let saved = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&poise_protocol::TrackCreationSaveRequest {
                            expected_version: context.version,
                            candidate,
                            preview_token: preview.preview_token,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = saved.status();
        let body = to_bytes(saved.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let saved: poise_protocol::TrackCreationContext = serde_json::from_slice(&body).unwrap();
        assert_eq!(saved.pending.len(), 1);
        assert_eq!(saved.pending[0].track_id, "gold");
        assert_eq!(
            state
                .http_state
                .runtime
                .committed_track_definitions()
                .await
                .len(),
            1
        );
        assert_eq!(crate::config::load_config(&path).unwrap().tracks.len(), 2);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let configuration: poise_protocol::TrackConfigurationResponse =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(
            configuration.version,
            state
                .http_state
                .configuration_store
                .as_ref()
                .unwrap()
                .current_version()
                .unwrap()
        );
        let put = app.clone().oneshot(Request::builder().method("PUT").uri("/tracks/btc-core/configuration").header("content-type", "application/json").body(Body::from(serde_json::json!({ "application_id": "apply-with-pending-track", "expected_version": configuration.version, "candidate": configuration.active }).to_string())).unwrap()).await.unwrap();
        let status = put.status();
        let body = to_bytes(put.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(
            audit
                .load_configuration_application("apply-with-pending-track")
                .unwrap()
                .is_some()
        );
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/track-creation")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        // Pending definitions survive process restart and disappear from the pending
        // list only once the normal startup path has installed them in runtime.
        let definitions = crate::config::load_config(&path)
            .unwrap()
            .track_definitions()
            .unwrap();
        assert_eq!(definitions[1].instrument().symbol, "GOLDUSDT");
    }

    fn router_with_web_root(state: HttpTestState, web_root: std::path::PathBuf) -> axum::Router {
        super::router_with_web_root(state.http_state, state.websocket_state, web_root)
    }

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }

    struct AccountSummaryOnlyExchange;

    struct UsdtAccountSummaryExchange;

    #[async_trait::async_trait]
    impl poise_application::exchange::AccountSummaryPort for AccountSummaryOnlyExchange {
        async fn get_account_summary(&self) -> anyhow::Result<AccountSummarySnapshot> {
            Ok(crate::test_support::test_account_summary_snapshot(
                "USD",
                12_500.0,
                Some(9_000.0),
                &[],
                Some(-350.0),
                Utc.with_ymd_and_hms(2026, 4, 4, 1, 23, 45).unwrap(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl poise_application::exchange::AccountSummaryPort for UsdtAccountSummaryExchange {
        async fn get_account_summary(&self) -> anyhow::Result<AccountSummarySnapshot> {
            Ok(crate::test_support::test_account_summary_snapshot(
                "USDT",
                12_500.0,
                Some(9_000.0),
                &[],
                Some(-350.0),
                Utc::now(),
            ))
        }
    }

    struct FailingQueryStore;

    #[async_trait::async_trait]
    impl TrackQueryStore for FailingQueryStore {
        async fn list_recent_track_activity(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<poise_application::StoredTrackActivity>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<StoredTrackEvent>> {
            Err(anyhow!("query unavailable"))
        }

        async fn list_recent_track_effects(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<PersistedTrackEffect>> {
            Err(anyhow!("query unavailable"))
        }

        async fn load_track_control_state(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Option<poise_application::TrackControlState>> {
            Err(anyhow!("query unavailable"))
        }

        async fn load_track_pnl_stats(
            &self,
            _track_id: &TrackId,
            _pnl_utc_day: chrono::NaiveDate,
        ) -> anyhow::Result<poise_engine::ledger::TrackPnlStats> {
            Err(anyhow!("query unavailable"))
        }

        async fn list_track_pnl_source_keys(
            &self,
            _track_id: &TrackId,
            _source_keys: &[String],
        ) -> anyhow::Result<Vec<String>> {
            Err(anyhow!("query unavailable"))
        }

        async fn load_track_updated_at(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
            Err(anyhow!("query unavailable"))
        }
    }

    async fn app_state() -> HttpTestState {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        build_test_state(repository, true).await
    }

    async fn app_state_without_market() -> HttpTestState {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        build_test_state(repository, false).await
    }

    fn mark_runtime_tasks_healthy(state: &HttpState) {
        for component in [
            RuntimeHealthComponent::MarketData,
            RuntimeHealthComponent::UserData,
            RuntimeHealthComponent::EffectWorker,
            RuntimeHealthComponent::Recovery,
            RuntimeHealthComponent::AccountMonitor,
        ] {
            state.runtime_health.record_success_now(component);
        }
        state.pnl_backfill_status.record_success_now();
    }

    async fn build_test_state<R>(repository: Arc<R>, observe_market: bool) -> HttpTestState
    where
        R: TrackMutationStore + TrackEffectJournal + TrackQueryStore + 'static,
    {
        let mut manager = test_manager_without_market();
        let mut snapshot = manager
            .get_track("btc-core")
            .cloned()
            .expect("seeded manager should expose runtime state");
        seed_runtime_pnl_stats(&mut snapshot);
        manager.install_planned_runtime(snapshot);
        if observe_market {
            observe_seed_market(&mut manager);
        }
        repository
            .commit_track_transition(
                "btc-core",
                None,
                &[DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                }],
            )
            .await
            .unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
        let query_store: Arc<dyn TrackQueryStore> = repository.clone();
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store.clone(),
            query_store.clone(),
            effect_store.clone(),
            notifications,
            account_margin_guard.clone(),
        );
        let query_service = Arc::new(TradingQueries::new(
            query_store.clone(),
            services.runtime.clone(),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store.clone(),
            services.runtime.clone(),
        ));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(services.notifications.clone());
        let account_projector = Arc::new(AccountProjector::new());
        HttpTestState {
            http_state: build_http_state(
                &services,
                query_store.clone(),
                query_service.clone(),
                debug_query_service,
                projector.clone(),
                account_monitor.clone(),
                account_projector.clone(),
            ),
            websocket_state: build_websocket_state(
                &services,
                Arc::new(TradingQueries::new(
                    repository as Arc<dyn TrackQueryStore>,
                    services.runtime.clone(),
                )),
                projector,
                account_monitor,
                account_projector,
            ),
            _configuration_dir: None,
        }
    }

    async fn build_test_state_with_recent_fills_account<R>(
        repository: Arc<R>,
        account: Arc<dyn AccountPort>,
    ) -> HttpTestState
    where
        R: TrackMutationStore + TrackEffectJournal + TrackQueryStore + 'static,
    {
        let mut manager = test_manager();
        let mut snapshot = manager
            .get_track("btc-core")
            .cloned()
            .expect("seeded manager should expose runtime state");
        seed_runtime_pnl_stats(&mut snapshot);
        manager.install_planned_runtime(snapshot);
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
        let query_store: Arc<dyn TrackQueryStore> = repository.clone();
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store,
            query_store.clone(),
            effect_store,
            notifications,
            account_margin_guard,
        );
        let query_service = Arc::new(TradingQueries::new(
            query_store.clone(),
            services.runtime.clone(),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store.clone(),
            services.runtime.clone(),
        ));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(services.notifications.clone());
        let account_projector = Arc::new(AccountProjector::new());
        HttpTestState {
            http_state: crate::assembly::build_http_state(
                Arc::clone(&services.runtime),
                query_service.clone(),
                debug_query_service,
                projector.clone(),
                Arc::new(crate::test_support::NoopMarketDataPort),
                account_monitor.clone(),
                account_projector.clone(),
                Arc::new(RuntimeHealth::new()),
                Arc::new(PnlBackfillStatus::new()),
                services.effect_journal_diagnostics.clone(),
                Arc::new(crate::exchange_freshness::ExchangeFreshness::default()),
                services.account_margin_guard.clone(),
                Arc::new(RecentFillsAuditor::new(account, query_store.clone())),
                Arc::new(std::collections::HashMap::new()),
                None,
                crate::server_context::SystemInfo {
                    exchange: "binance".to_string(),
                    deployment: "testnet".to_string(),
                    instance_directory: "test-instance".to_string(),
                    server_version: env!("CARGO_PKG_VERSION").to_string(),
                    bind_address: "127.0.0.1:0".to_string(),
                    credentials_configured: true,
                },
            ),
            websocket_state: build_websocket_state(
                &services,
                query_service,
                projector,
                account_monitor,
                account_projector,
            ),
            _configuration_dir: None,
        }
    }

    async fn app_state_with_account_summary() -> HttpTestState {
        app_state_with_account_summary_from_manager(
            test_manager(),
            Arc::new(AccountSummaryOnlyExchange),
        )
        .await
    }

    async fn app_state_with_usdt_account_summary() -> HttpTestState {
        app_state_with_account_summary_from_manager(
            test_manager(),
            Arc::new(UsdtAccountSummaryExchange),
        )
        .await
    }

    async fn app_state_with_account_summary_from_manager(
        mut manager: TrackManager,
        account_port: Arc<dyn poise_application::exchange::AccountSummaryPort>,
    ) -> HttpTestState {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let mut snapshot = manager
            .get_track("btc-core")
            .cloned()
            .expect("seeded manager should expose runtime state");
        seed_runtime_pnl_stats(&mut snapshot);
        manager.install_planned_runtime(snapshot);
        repository
            .commit_track_transition(
                "btc-core",
                None,
                &[DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                }],
            )
            .await
            .unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
        let query_store: Arc<dyn TrackQueryStore> = repository.clone();
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store,
            query_store.clone(),
            effect_store,
            notifications.clone(),
            account_margin_guard,
        );
        let account_store: Arc<dyn AccountMonitorStore> =
            Arc::new(SqliteStorage::in_memory().unwrap());
        account_store
            .save_state(&StoredAccountMonitorState {
                trading_day: chrono::NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
                baseline_equity: crate::test_support::test_amount(13_000.0, "USD"),
                baseline_captured_at: Utc.with_ymd_and_hms(2026, 4, 4, 0, 0, 1).unwrap(),
            })
            .await
            .unwrap();
        let account_monitor = AccountMonitor::restore(
            account_port,
            account_store,
            notifications,
            AccountMonitorConfig::default(),
        )
        .await
        .unwrap();
        account_monitor.refresh_once().await.unwrap();
        let account_monitor = Arc::new(account_monitor);
        let projector = Arc::new(TrackProjector::new());
        let account_projector = Arc::new(AccountProjector::new());
        let query_service = Arc::new(TradingQueries::new(
            query_store.clone(),
            services.runtime.clone(),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store.clone(),
            services.runtime.clone(),
        ));
        HttpTestState {
            http_state: build_http_state(
                &services,
                query_store.clone(),
                query_service.clone(),
                debug_query_service,
                projector.clone(),
                account_monitor.clone(),
                account_projector.clone(),
            ),
            websocket_state: build_websocket_state(
                &services,
                query_service,
                projector,
                account_monitor,
                account_projector,
            ),
            _configuration_dir: None,
        }
    }

    fn test_manager() -> TrackManager {
        let mut manager = test_manager_without_market();
        observe_seed_market(&mut manager);
        manager
    }

    fn no_binding_test_manager() -> TrackManager {
        let mut manager = test_manager_without_market();
        manager
            .observe_market_mutation(
                &TrackId::new("btc-core"),
                poise_engine::observation::MarketObservation::ExecutionQuote {
                    execution_quote: poise_application::exchange::ExecutionQuote {
                        best_bid: 100.0,
                        best_ask: 100.0,
                    },
                },
                chrono::Utc::now(),
            )
            .unwrap();
        manager
    }

    fn test_manager_without_market() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(FakeClock));
        manager
            .add_track(
                TrackDefinition::try_new(
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
                        out_of_band_policy: BandProtectionPolicy::Freeze,
                        risk_acquisition: Default::default(),
                    },
                    Some(3000.0),
                    LossLimits {
                        daily_loss_limit: 100.0,
                        total_loss_limit: 300.0,
                    },
                    None,
                )
                .unwrap(),
                crate::test_support::test_instrument_spec(),
            )
            .unwrap();
        manager
    }

    fn inverse_test_manager_without_mark() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(FakeClock));
        let usd = Asset::try_new("USD").unwrap();
        let definition = TrackDefinition::try_new(
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
                out_of_band_policy: BandProtectionPolicy::Freeze,
                risk_acquisition: Default::default(),
            },
            Some(3000.0),
            LossLimits {
                daily_loss_limit: 100.0,
                total_loss_limit: 300.0,
            },
            None,
        )
        .unwrap();
        let spec = InstrumentSpec::try_inverse(
            Instrument::new(Venue::Binance, "BTCUSDT"),
            Asset::try_new("BTC").unwrap(),
            usd.clone(),
            usd.clone(),
            Amount::new(100.0, usd.clone()).unwrap(),
            poise_core::types::PricePrecision::FixedTick,
            0.1,
            1.0,
            1.0,
            Amount::new(1.0, usd).unwrap(),
        )
        .unwrap();
        manager.add_track(definition, spec).unwrap();
        observe_seed_market(&mut manager);
        manager
    }

    fn observe_seed_market(manager: &mut TrackManager) {
        manager
            .observe_market_mutation(
                &TrackId::new("btc-core"),
                poise_engine::observation::MarketObservation::ExecutionQuote {
                    execution_quote: poise_application::exchange::ExecutionQuote {
                        best_bid: 95.0,
                        best_ask: 95.0,
                    },
                },
                chrono::Utc::now(),
            )
            .unwrap();
    }

    fn seed_runtime_pnl_stats(snapshot: &mut poise_engine::runtime::TrackRuntime) {
        snapshot.set_unrealized_pnl(265.2);
        let mut pnl_stats = snapshot.pnl_stats().clone();
        pnl_stats.pnl_utc_day = chrono::NaiveDate::from_ymd_opt(2026, 3, 24).unwrap();
        pnl_stats.gross_realized_pnl_today = 980.1;
        pnl_stats.gross_realized_pnl_cumulative = 980.1;
        pnl_stats.trading_fee_cumulative = 12.3;
        pnl_stats.funding_fee_cumulative = -4.0;
        snapshot.replace_pnl_stats(pnl_stats);
    }

    #[tokio::test]
    async fn router_accepts_http_state_without_runtime_dependencies() {
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
    }

    #[tokio::test]
    async fn configuration_get_returns_safe_projection_and_stable_version() {
        let app = router(app_state().await);
        let request = || {
            Request::builder()
                .uri("/tracks/btc-core/configuration")
                .body(Body::empty())
                .unwrap()
        };
        let first = app.clone().oneshot(request()).await.unwrap();
        let second = app.oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first: serde_json::Value =
            serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let second: serde_json::Value =
            serde_json::from_slice(&to_bytes(second.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(first["version"], second["version"]);
        assert_eq!(first["active"]["notional_per_unit"], 375.0);
        assert!(first.get("exchange").is_none());
        assert!(first.get("secret").is_none());
    }

    #[tokio::test]
    async fn system_returns_safe_instance_metadata_without_credentials() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["exchange"], "binance");
        assert_eq!(value["deployment"], "testnet");
        assert_eq!(value["credentials_configured"], true);
        assert!(value["recent_configuration_conflict"].is_null());
        let serialized = String::from_utf8(body.to_vec()).unwrap();
        for sensitive in ["test-key", "test-secret", "api_key", "api_secret"] {
            assert!(!serialized.contains(sensitive), "leaked {sensitive}");
        }
    }

    #[tokio::test]
    async fn system_reports_the_most_recent_external_configuration_conflict() {
        let (state, audit, _path, _directory) = with_configuration_store(app_state().await);
        let observed_at = Utc::now();
        audit
            .record_configuration_application(
                &poise_storage::sqlite::ConfigurationApplicationAuditRow {
                    application_id: "apply-external-change".to_string(),
                    request_hash: "hash".to_string(),
                    track_id: "btc-core".to_string(),
                    old_version: "old".to_string(),
                    candidate_version: "candidate".to_string(),
                    result: poise_storage::sqlite::ConfigurationApplicationAuditStatus::Preparing,
                    change_fields: Some(vec!["upper_price".to_string()]),
                    failure_reason: None,
                    created_at: observed_at,
                    updated_at: observed_at,
                },
            )
            .unwrap();
        audit
            .record_configuration_application_failure(
                "apply-external-change",
                "external_change",
                observed_at,
            )
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value["recent_configuration_conflict"]["application_id"],
            "apply-external-change"
        );
        assert_eq!(
            value["recent_configuration_conflict"]["track_id"],
            "btc-core"
        );
        assert_eq!(
            value["recent_configuration_conflict"]["observed_at"],
            observed_at.to_rfc3339()
        );
    }

    #[tokio::test]
    async fn configuration_get_returns_404_for_unknown_track() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/tracks/missing/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn configuration_get_projects_interrupted_audit_as_explicit_same_id_retry() {
        let (state, audit, _path, _directory) = with_configuration_store(app_state().await);
        let now = Utc::now();
        audit
            .record_configuration_application(
                &poise_storage::sqlite::ConfigurationApplicationAuditRow {
                    application_id: "apply-interrupted".to_string(),
                    request_hash: "hash".to_string(),
                    track_id: "btc-core".to_string(),
                    old_version: "old".to_string(),
                    candidate_version: "candidate".to_string(),
                    result: poise_storage::sqlite::ConfigurationApplicationAuditStatus::Interrupted,
                    change_fields: None,
                    failure_reason: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["application"]["status"], "failed");
        assert_eq!(body["application"]["accepted"], false);
        assert_eq!(body["application"]["retryable"], true);
        assert!(
            body["application"]["message"]
                .as_str()
                .unwrap()
                .contains("same application ID")
        );
    }

    #[tokio::test]
    async fn configuration_put_applies_atomically_and_is_idempotent() {
        let (state, audit, path, _directory) = with_configuration_store(
            app_state_with_account_summary_from_manager(
                no_binding_test_manager(),
                Arc::new(UsdtAccountSummaryExchange),
            )
            .await,
        );
        let runtime = state.http_state.runtime.clone();
        let mut unavailable_http_state = state.http_state.clone();
        let (unavailable_notifications, _) =
            tokio::sync::broadcast::channel::<ApplicationNotification>(1);
        unavailable_http_state.account_monitor =
            unavailable_account_monitor(unavailable_notifications);
        let retry_app = router(HttpTestState {
            http_state: unavailable_http_state,
            websocket_state: state.websocket_state.clone(),
            _configuration_dir: None,
        });
        let app = router(state);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let initial: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let mut candidate = initial["active"].clone();
        candidate["notional_per_unit"] = serde_json::json!(300.0);
        let request = serde_json::json!({ "application_id": "apply-http-1", "expected_version": initial["version"], "candidate": candidate });
        let apply = || {
            Request::builder()
                .method("PUT")
                .uri("/tracks/btc-core/configuration")
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap()
        };
        let first = app.clone().oneshot(apply()).await.unwrap();
        let first_status = first.status();
        let first_body = to_bytes(first.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            first_status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&first_body)
        );
        let accepted: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
        assert_eq!(accepted["application_id"], "apply-http-1");
        assert_eq!(accepted["status"], "active");
        assert_eq!(
            accepted["active_version"].as_str(),
            Some(accepted["active_version"].as_str().unwrap())
        );
        let parsed = crate::config::load_config(&path).unwrap();
        assert_eq!(parsed.tracks[0].notional_per_unit, 300.0);
        assert_eq!(
            runtime.committed_track_definitions().await[0]
                .track_config()
                .notional_per_unit,
            300.0
        );
        let after = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let after: serde_json::Value =
            serde_json::from_slice(&to_bytes(after.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(after["version"], accepted["active_version"]);
        assert_eq!(after["application"]["application_id"], "apply-http-1");
        assert_eq!(after["application_status"], "active");
        audit
            .update_configuration_application_result(
                "apply-http-1",
                poise_storage::sqlite::ConfigurationApplicationAuditStatus::Preparing,
                Utc::now(),
            )
            .unwrap();
        // The stored request must win over newly unavailable capacity facts.
        let retry = retry_app.clone().oneshot(apply()).await.unwrap();
        assert_eq!(retry.status(), StatusCode::OK);
        assert_eq!(
            audit
                .load_configuration_application("apply-http-1")
                .unwrap()
                .unwrap()
                .result,
            poise_storage::sqlite::ConfigurationApplicationAuditStatus::Active
        );
        let mut conflicting = request.clone();
        conflicting["candidate"]["notional_per_unit"] = serde_json::json!(250.0);
        let conflict = retry_app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/tracks/btc-core/configuration")
                    .header("content-type", "application/json")
                    .body(Body::from(conflicting.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn configuration_put_rejects_stale_version_without_side_effects() {
        let (state, audit, path, _directory) = with_configuration_store(
            app_state_with_account_summary_from_manager(
                no_binding_test_manager(),
                Arc::new(UsdtAccountSummaryExchange),
            )
            .await,
        );
        let runtime = state.http_state.runtime.clone();
        let before_definition = runtime.committed_track_definitions().await;
        let before_file = std::fs::read_to_string(&path).unwrap();
        let app = router(state);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let response = app.oneshot(Request::builder().method("PUT").uri("/tracks/btc-core/configuration").header("content-type", "application/json").body(Body::from(serde_json::json!({"application_id":"apply-stale", "expected_version":"stale", "candidate":value["active"]}).to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            audit
                .load_configuration_application("apply-stale")
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), before_file);
        assert_eq!(
            runtime.committed_track_definitions().await,
            before_definition
        );
    }

    #[tokio::test]
    async fn configuration_put_accepts_active_binding_and_reports_waiting_for_orders() {
        let (state, audit, path, _directory) =
            with_configuration_store(app_state_with_usdt_account_summary().await);
        let before_file = std::fs::read_to_string(&path).unwrap();
        let app = router(state);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let response = app.oneshot(Request::builder().method("PUT").uri("/tracks/btc-core/configuration").header("content-type", "application/json").body(Body::from(serde_json::json!({"application_id":"apply-bound", "expected_version":value["version"], "candidate":value["active"]}).to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let accepted: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(accepted["application_id"], "apply-bound");
        assert_eq!(accepted["status"], "waiting_for_orders");
        assert_eq!(accepted["accepted"], true);
        assert!(accepted["active_version"].is_null());
        assert_eq!(
            audit
                .load_configuration_application("apply-bound")
                .unwrap()
                .unwrap()
                .result,
            poise_storage::sqlite::ConfigurationApplicationAuditStatus::WaitingForOrders
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), before_file);
    }

    #[tokio::test]
    async fn configuration_delete_abandons_pending_application_without_claiming_old_definition_is_ready()
     {
        let (state, audit, _path, _directory) =
            with_configuration_store(app_state_with_usdt_account_summary().await);
        let app = router(state);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let accepted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/tracks/btc-core/configuration")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "application_id": "apply-abandon",
                            "expected_version": value["version"],
                            "candidate": value["active"],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/tracks/btc-core/configuration/application/apply-abandon")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["status"], "waiting_for_orders");
        assert_eq!(body["accepted"], false);
        assert!(body["message"].as_str().unwrap().contains("old orders"));
        assert_eq!(
            audit
                .load_configuration_application("apply-abandon")
                .unwrap()
                .unwrap()
                .result,
            poise_storage::sqlite::ConfigurationApplicationAuditStatus::WaitingForOrders
        );
    }

    #[tokio::test]
    async fn configuration_put_rejects_another_track_while_an_application_is_waiting() {
        let (state, audit, _path, _directory) =
            with_configuration_store(app_state_with_usdt_account_summary().await);
        let now = Utc::now();
        audit
            .record_configuration_application(
                &poise_storage::sqlite::ConfigurationApplicationAuditRow {
                    application_id: "apply-btc".to_string(),
                    request_hash: "hash-btc".to_string(),
                    track_id: "btc-core".to_string(),
                    old_version: "old".to_string(),
                    candidate_version: "next".to_string(),
                    result: poise_storage::sqlite::ConfigurationApplicationAuditStatus::WaitingForOrders,
                    change_fields: None,
                    failure_reason: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .unwrap();
        let app = router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/tracks/eth-core/configuration")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "application_id": "apply-eth",
                            "expected_version": "old",
                            "candidate": serde_json::json!({
                                "lower_price": 90.0,
                                "upper_price": 110.0,
                                "long_exposure_units": 8.0,
                                "short_exposure_units": 8.0,
                                "notional_per_unit": 375.0,
                                "min_rebalance_units": 0.5,
                                "shape_family": "linear",
                                "out_of_band_policy": "freeze",
                                "max_notional": 3000.0,
                                "daily_loss_limit": 100.0,
                                "total_loss_limit": 300.0,
                                "risk_acquisition": {
                                    "initial_ratio": 0.0,
                                    "advantage_steps": 0.0,
                                    "min_release_steps": 0.0,
                                    "max_release_steps": 0.0,
                                    "catchup_ratio": 0.0,
                                    "stale_release_minutes": 0
                                }
                            })
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn configuration_put_marks_preparing_audit_failed_when_runtime_has_old_definition() {
        let (state, audit, _path, _directory) = with_configuration_store(
            app_state_with_account_summary_from_manager(
                no_binding_test_manager(),
                Arc::new(UsdtAccountSummaryExchange),
            )
            .await,
        );
        let store = state.http_state.configuration_store.clone().unwrap();
        let app = router(state);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let current: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let mut candidate = current["active"].clone();
        candidate["notional_per_unit"] = serde_json::json!(300.0);
        let candidate_view: TrackConfigurationView =
            serde_json::from_value(candidate.clone()).unwrap();
        let request_hash = super::configuration_request_hash(
            "btc-core",
            current["version"].as_str().unwrap(),
            &candidate_view,
        );
        let candidate_version = store
            .candidate_version("btc-core", &candidate_view)
            .unwrap();
        let now = Utc::now();
        audit
            .record_configuration_application(
                &poise_storage::sqlite::ConfigurationApplicationAuditRow {
                    application_id: "apply-preparing-old".to_string(),
                    request_hash,
                    track_id: "btc-core".to_string(),
                    old_version: current["version"].as_str().unwrap().to_string(),
                    candidate_version,
                    result: poise_storage::sqlite::ConfigurationApplicationAuditStatus::Preparing,
                    change_fields: None,
                    failure_reason: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .unwrap();
        let response = app.oneshot(Request::builder().method("PUT").uri("/tracks/btc-core/configuration").header("content-type", "application/json").body(Body::from(serde_json::json!({"application_id":"apply-preparing-old", "expected_version":current["version"], "candidate":candidate}).to_string())).unwrap()).await.unwrap();
        assert_ne!(response.status(), StatusCode::OK);
        assert_eq!(
            audit
                .load_configuration_application("apply-preparing-old")
                .unwrap()
                .unwrap()
                .result,
            poise_storage::sqlite::ConfigurationApplicationAuditStatus::Failed
        );
    }

    #[tokio::test]
    async fn configuration_preview_fails_closed_without_account_capacity() {
        let app = router(app_state().await);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/configuration/preview")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"candidate": value["active"]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["valid"], false);
        assert_eq!(body["blockers"][0]["code"], "capacity_proof_unavailable");
    }

    #[tokio::test]
    async fn failed_same_id_retry_rechecks_latest_capacity_before_runtime_side_effects() {
        let (state, audit, path, _directory) = with_configuration_store(app_state().await);
        let runtime = state.http_state.runtime.clone();
        let store = state.http_state.configuration_store.clone().unwrap();
        let app = router(state);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let current: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let mut candidate = current["active"].clone();
        candidate["notional_per_unit"] = serde_json::json!(300.0);
        let candidate_view: TrackConfigurationView =
            serde_json::from_value(candidate.clone()).unwrap();
        let application_id = "apply-failed-capacity";
        let request_hash = super::configuration_request_hash(
            "btc-core",
            current["version"].as_str().unwrap(),
            &candidate_view,
        );
        audit
            .record_configuration_application(
                &poise_storage::sqlite::ConfigurationApplicationAuditRow {
                    application_id: application_id.to_string(),
                    request_hash,
                    track_id: "btc-core".to_string(),
                    old_version: current["version"].as_str().unwrap().to_string(),
                    candidate_version: store
                        .candidate_version("btc-core", &candidate_view)
                        .unwrap(),
                    result: poise_storage::sqlite::ConfigurationApplicationAuditStatus::Failed,
                    change_fields: None,
                    failure_reason: Some("runtime_rejected".to_string()),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            )
            .unwrap();
        let before_file = std::fs::read_to_string(&path).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/tracks/btc-core/configuration")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "application_id": application_id,
                            "expected_version": current["version"],
                            "candidate": candidate,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before_file);
        assert_eq!(
            runtime.committed_track_definitions().await[0]
                .track_config()
                .notional_per_unit,
            375.0
        );
        assert_eq!(
            audit
                .load_configuration_application(application_id)
                .unwrap()
                .unwrap()
                .result,
            poise_storage::sqlite::ConfigurationApplicationAuditStatus::Failed
        );
    }

    #[tokio::test]
    async fn configuration_preview_reports_specific_invalid_candidate_field() {
        let app = router(app_state().await);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        value["active"]["notional_per_unit"] = serde_json::json!(0.0);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/configuration/preview")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"candidate": value["active"]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["field_errors"][0]["field"], "notional_per_unit");
    }

    #[tokio::test]
    async fn configuration_preview_success_projects_policy_risk_and_capacity() {
        let app = router(app_state_with_usdt_account_summary().await);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/configuration/preview")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"candidate": value["active"]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["valid"], true, "{body}");
        assert_eq!(body["impact"]["candidate_policy_state"], "in_band");
        assert_eq!(body["impact"]["candidate_risk_outcome"], "allowed");
        assert!(
            body["capacity_proof"]["available_increase_notional"]
                .as_f64()
                .unwrap()
                > 0.0
        );
    }

    #[tokio::test]
    async fn configuration_preview_rejects_non_live_strategy_price() {
        let state = app_state_without_market().await;
        let app = router(state);
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/configuration/preview")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"candidate": value["active"]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["blockers"][0]["code"], "market_data_unavailable");
    }

    #[tokio::test]
    async fn configuration_preview_fails_closed_for_inverse_without_mark_price() {
        let app = router(
            app_state_with_account_summary_from_manager(
                inverse_test_manager_without_mark(),
                Arc::new(UsdtAccountSummaryExchange),
            )
            .await,
        );
        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/configuration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/configuration/preview")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"candidate": value["active"]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["blockers"][0]["code"], "market_data_unavailable");
        assert!(
            body["blockers"][0]["message"]
                .as_str()
                .unwrap()
                .contains("inverse capacity proof")
        );
    }

    #[test]
    fn configuration_error_field_keeps_cross_release_error_on_max_release_steps() {
        assert_eq!(
            super::configuration_error_field("max_release_steps must be >= min_release_steps"),
            "risk_acquisition.max_release_steps"
        );
    }

    #[tokio::test]
    async fn static_web_root_serves_index_and_spa_fallback_without_shadowing_api_routes() {
        let temporary_web_root = tempfile::tempdir().unwrap();
        fs::write(
            temporary_web_root.path().join("index.html"),
            "<main>Poise control console</main>",
        )
        .unwrap();
        fs::create_dir(temporary_web_root.path().join("assets")).unwrap();
        fs::write(
            temporary_web_root.path().join("assets/app.js"),
            "console.log('poise');",
        )
        .unwrap();
        let app = router_with_web_root(app_state().await, temporary_web_root.path().to_path_buf());

        for uri in ["/", "/tracks/btc-core/overview"] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(body.as_ref(), b"<main>Poise control console</main>");
        }

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/assets/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"console.log('poise');");

        let response = app
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
        assert!(payload.items[0].execution.active_binding_count > 0);
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
        assert_eq!(payload["tasks"].as_array().unwrap().len(), 6);
        let pnl_backfill = payload["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["component"] == "pnl_backfill")
            .unwrap();
        assert_eq!(pnl_backfill["status"], "unknown");
        assert!(pnl_backfill["last_success_at"].is_null());
        assert!(pnl_backfill["last_error"].is_null());
        assert!(payload["pnl_backfill"]["last_completed_at"].is_null());
        assert_eq!(payload["pnl_backfill"]["available_tracks"], 0);
        assert_eq!(payload["pnl_backfill"]["unsupported_tracks"], 0);
        assert_eq!(payload["pnl_backfill"]["records_seen"], 0);
        assert_eq!(payload["pnl_backfill"]["records_inserted"], 0);
        assert_eq!(payload["pnl_backfill"]["records_skipped"], 0);
        assert_eq!(payload["pnl_backfill"]["failures"], 0);
        assert!(payload["pnl_backfill"]["last_error_at"].is_null());
        assert!(payload["pnl_backfill"]["last_error"].is_null());
    }

    #[tokio::test]
    async fn readiness_blocks_unknown_critical_tasks_while_health_keeps_observing() {
        let response = router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TradingReadinessResponse = serde_json::from_slice(&body).unwrap();
        assert!(!payload.ready_to_trade);
        for component in [
            "market_data",
            "user_data",
            "effect_worker",
            "recovery",
            "account_monitor",
            "pnl_backfill",
        ] {
            assert!(
                payload
                    .blockers
                    .iter()
                    .any(|issue| issue.component == component),
                "missing readiness blocker for {component}"
            );
        }
    }

    #[tokio::test]
    async fn readiness_returns_ok_after_all_current_facts_are_healthy() {
        let state = app_state_with_account_summary().await;
        mark_runtime_tasks_healthy(&state.http_state);

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TradingReadinessResponse = serde_json::from_slice(&body).unwrap();
        assert!(payload.ready_to_trade);
        assert!(payload.blockers.is_empty());
    }

    #[tokio::test]
    async fn readiness_blocks_on_durable_configuration_application_not_only_runtime_memory() {
        let (state, audit, _path, _directory) =
            with_configuration_store(app_state_with_account_summary().await);
        mark_runtime_tasks_healthy(&state.http_state);
        let now = Utc::now();
        audit
            .record_configuration_application(
                &poise_storage::sqlite::ConfigurationApplicationAuditRow {
                    application_id: "apply-durable-wait".to_string(),
                    request_hash: "hash".to_string(),
                    track_id: "btc-core".to_string(),
                    old_version: "old".to_string(),
                    candidate_version: "candidate".to_string(),
                    result: poise_storage::sqlite::ConfigurationApplicationAuditStatus::WaitingForOrders,
                    change_fields: None,
                    failure_reason: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .unwrap();

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TradingReadinessResponse = serde_json::from_slice(&body).unwrap();
        assert!(payload.blockers.iter().any(|issue| {
            issue.component == "configuration_application"
                && issue.reason.contains("apply-durable-wait")
        }));
    }

    #[tokio::test]
    async fn readiness_blocks_while_exchange_state_requires_reconciliation() {
        let state = app_state_with_account_summary().await;
        mark_runtime_tasks_healthy(&state.http_state);
        state
            .http_state
            .exchange_freshness
            .mark_stale(
                "btc-core",
                crate::exchange_freshness::ExchangeFreshnessReason::SubmitOutcomeUnknown,
            )
            .await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TradingReadinessResponse = serde_json::from_slice(&body).unwrap();
        assert!(payload.blockers.iter().any(|issue| {
            issue.component == "exchange_state"
                && issue.reason.contains("submit outcome is unknown")
        }));
    }

    #[tokio::test]
    async fn readiness_keeps_effect_journal_failure_as_non_blocking_warning() {
        let state = app_state_with_account_summary().await;
        mark_runtime_tasks_healthy(&state.http_state);
        state
            .http_state
            .effect_journal_diagnostics
            .record_failure_for_test(
                &TrackId::new("btc-core"),
                "append_entries",
                "journal disk is read-only",
            );

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TradingReadinessResponse = serde_json::from_slice(&body).unwrap();
        assert!(payload.ready_to_trade);
        assert!(payload.blockers.is_empty());
        assert!(payload.warnings.iter().any(|issue| {
            issue.component == "effect_journal"
                && issue.reason.contains("journal disk is read-only")
        }));
    }

    #[tokio::test]
    async fn readiness_reads_the_active_account_margin_guard_directly() {
        let state = app_state_with_account_summary().await;
        mark_runtime_tasks_healthy(&state.http_state);
        state
            .http_state
            .account_margin_guard
            .activate_insufficient_margin(
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                "exchange rejected submit for insufficient margin",
                Utc::now(),
            );

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/readiness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TradingReadinessResponse = serde_json::from_slice(&body).unwrap();
        assert!(payload.blockers.iter().any(|issue| {
            issue.component == "account_capacity" && issue.reason.contains("insufficient margin")
        }));
    }

    #[tokio::test]
    async fn health_returns_service_unavailable_when_attention_required_present() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let mut manager = test_manager();
        let mut snapshot = manager
            .get_track("btc-core")
            .cloned()
            .expect("seeded manager should expose runtime state");
        snapshot.set_market_data_stale_since(Some(Utc::now()));
        manager.install_planned_runtime(snapshot);
        let state = {
            let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
            let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
            let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
            let query_store: Arc<dyn TrackQueryStore> = repository.clone();
            let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
            let services = build_test_application_services(
                manager,
                mutation_store.clone(),
                query_store.clone(),
                effect_store.clone(),
                notifications,
                account_margin_guard.clone(),
            );
            let query_service = Arc::new(TradingQueries::new(
                query_store.clone(),
                services.runtime.clone(),
            ));
            let debug_query_service = Arc::new(TrackDebugQueryService::new(
                query_store.clone(),
                services.runtime.clone(),
            ));
            let projector = Arc::new(TrackProjector::new());
            let account_monitor = unavailable_account_monitor(services.notifications.clone());
            let account_projector = Arc::new(AccountProjector::new());
            HttpTestState {
                http_state: build_http_state(
                    &services,
                    query_store.clone(),
                    query_service.clone(),
                    debug_query_service,
                    projector.clone(),
                    account_monitor.clone(),
                    account_projector.clone(),
                ),
                websocket_state: build_websocket_state(
                    &services,
                    Arc::new(TradingQueries::new(
                        repository as Arc<dyn TrackQueryStore>,
                        services.runtime.clone(),
                    )),
                    projector,
                    account_monitor,
                    account_projector,
                ),
                _configuration_dir: None,
            }
        };

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
    async fn health_returns_service_unavailable_when_runtime_task_degraded() {
        let state = app_state().await;
        state.http_state.runtime_health.record_error(
            RuntimeHealthComponent::Recovery,
            Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap(),
            "temporary okx outage",
        );

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
        assert_eq!(payload["attention_required_count"], 0);
        let recovery = payload["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["component"] == "recovery")
            .unwrap();
        assert_eq!(recovery["status"], "degraded");
        assert_eq!(recovery["last_error_at"], "2026-06-17T01:02:03+00:00");
        assert_eq!(recovery["last_error"], "temporary okx outage");
    }

    #[tokio::test]
    async fn health_returns_pnl_backfill_observability() {
        let state = app_state().await;
        let observed_at = Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap();
        state.http_state.pnl_backfill_status.replace_snapshot(
            crate::runtime::PnlBackfillSnapshot {
                last_completed_at: Some(observed_at),
                last_success_at: None,
                available_tracks: 2,
                unsupported_tracks: 1,
                incomplete_tracks: 0,
                records_seen: 5,
                records_inserted: 2,
                records_skipped: 3,
                failures: 1,
                last_error_at: Some(observed_at),
                last_error: Some("failed to persist backfilled track pnl record".to_string()),
            },
        );

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
        assert_eq!(
            payload["pnl_backfill"]["last_completed_at"],
            "2026-06-17T01:02:03+00:00"
        );
        assert_eq!(payload["pnl_backfill"]["available_tracks"], 2);
        assert_eq!(payload["pnl_backfill"]["unsupported_tracks"], 1);
        assert_eq!(payload["pnl_backfill"]["records_seen"], 5);
        assert_eq!(payload["pnl_backfill"]["records_inserted"], 2);
        assert_eq!(payload["pnl_backfill"]["records_skipped"], 3);
        assert_eq!(payload["pnl_backfill"]["failures"], 1);
        let pnl_backfill = payload["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["component"] == "pnl_backfill")
            .unwrap();
        assert_eq!(pnl_backfill["status"], "degraded");
        assert_eq!(
            payload["pnl_backfill"]["last_error_at"],
            "2026-06-17T01:02:03+00:00"
        );
        assert_eq!(
            payload["pnl_backfill"]["last_error"],
            "failed to persist backfilled track pnl record"
        );
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
        assert_eq!(payload.position_to_execution_gap, Some(2.0));
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
        assert_eq!(
            payload_json["pnl"]["gross_realized_pnl"].as_f64(),
            Some(980.1)
        );
        assert!((payload_json["pnl"]["net_realized_pnl"].as_f64().unwrap() - 963.8).abs() < 1e-9);
        assert!((payload_json["pnl"]["total_pnl"].as_f64().unwrap() - 1229.0).abs() < 1e-9);
        assert_eq!(payload_json["pnl"]["unrealized_pnl"].as_f64(), Some(265.2));
        assert_eq!(
            payload.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert!(payload.execution.active_binding_count > 0);
        assert_eq!(
            payload.execution.active_binding_count,
            payload.execution.bindings.len() as u32
        );
        assert!(
            payload
                .execution
                .bindings
                .iter()
                .all(|binding| binding.label.starts_with("maker ")
                    || binding.label.starts_with("target "))
        );
        assert!(
            payload
                .execution
                .bindings
                .iter()
                .all(|binding| binding.status == ExecutionBindingStatusView::SubmitPending)
        );
        assert!(payload.execution.bindings.iter().all(|binding| matches!(
            binding.intent,
            ExecutionBindingIntentView::IncreaseInventory
                | ExecutionBindingIntentView::DecreaseInventory
        )));
        assert!(!payload.available_commands.is_empty());
        assert!(
            payload_json["execution"]["bindings"][0]
                .get("phase")
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
    async fn get_track_chart_returns_exchange_candles() {
        let mut state = app_state().await;
        state.http_state.market_data = Arc::new(ChartMarketData);
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/tracks/btc-core/chart?interval=five_minutes&limit=20")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TrackChartResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.candles.len(), 1);
        assert_eq!(payload.candles[0].close, 102.0);
    }

    #[tokio::test]
    async fn get_account_returns_latest_summary() {
        let response = router(app_state_with_account_summary().await)
            .oneshot(
                Request::builder()
                    .uri("/account")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: AccountSummaryView = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.equity, Some(12_500.0));
        assert_eq!(payload.available, Some(9_000.0));
        assert_eq!(payload.unrealized_pnl, Some(-350.0));
        assert_eq!(payload.day_change_pct, Some(-3.8461538461538463));
        assert_eq!(payload.risk_signal, RiskSignalView::Attention);
        assert_eq!(payload.reason.as_deref(), Some("day_change -3.8%"));
        assert_eq!(
            payload.day_base_at.as_deref(),
            Some("2026-04-04T00:00:01+00:00")
        );
        assert_eq!(
            payload.updated_at.as_deref(),
            Some("2026-04-04T01:23:45+00:00")
        );

        let analysis = payload
            .analysis
            .expect("account analysis should be present");
        assert_eq!(analysis.tracks.len(), 1);
        assert_eq!(analysis.tracks[0].track_id, "btc-core");
        assert_eq!(analysis.tracks[0].settlement_asset, "USDT");
        assert_eq!(analysis.tracks[0].pnl_asset, "USDT");
    }

    #[tokio::test]
    async fn get_account_keeps_summary_when_analysis_query_fails() {
        let mut state = app_state_with_account_summary().await;
        state.http_state.query_service = Arc::new(TradingQueries::new(
            Arc::new(FailingQueryStore),
            state.websocket_state.runtime.clone(),
        ));

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/account")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: AccountSummaryView = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.equity, Some(12_500.0));
        assert_eq!(payload.available, Some(9_000.0));
        assert_eq!(payload.analysis, None);
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
    async fn submit_command_keeps_committed_detail_when_persistence_fails() {
        let repository = Arc::new(FailingRepository::default());
        let manager = test_manager();
        repository.seed_track(&TrackId::new("btc-core"));
        let (notifications, _) = tokio::sync::broadcast::channel::<ApplicationNotification>(16);
        let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
        let effect_store = repository.clone() as Arc<dyn TrackEffectJournal>;
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store,
            repository.clone() as Arc<dyn TrackQueryStore>,
            effect_store,
            notifications,
            account_margin_guard,
        );
        let query_store = repository.clone() as Arc<dyn TrackQueryStore>;
        let query_service = Arc::new(TradingQueries::new(
            query_store.clone(),
            services.runtime.clone(),
        ));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(services.notifications.clone());
        let account_projector = Arc::new(AccountProjector::new());
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store.clone(),
            services.runtime.clone(),
        ));
        let app = router(HttpTestState {
            http_state: build_http_state(
                &services,
                query_store.clone(),
                query_service.clone(),
                debug_query_service,
                projector.clone(),
                account_monitor.clone(),
                account_projector.clone(),
            ),
            websocket_state: build_websocket_state(
                &services,
                query_service,
                projector,
                account_monitor,
                account_projector,
            ),
            _configuration_dir: None,
        });

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
        let payload: TrackDetailView = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.status.lifecycle.status, TrackStatus::Paused);
        assert_eq!(payload.decision.revision, None);
        assert_eq!(payload.decision.execution_target, None);
        assert_eq!(payload.position_to_execution_gap, None);
        assert_eq!(payload.decision.reason, DecisionReasonView::Paused);
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

    #[tokio::test]
    async fn get_recent_fills_audit_compares_exchange_and_local_pnl_records() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let track_id = TrackId::new("btc-core");
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        TrackIdentityStore::guard_track_identities(
            &*repository,
            &[(track_id.clone(), instrument.clone())],
        )
        .await
        .unwrap();
        let recorded = audit_trade_record(
            instrument.clone(),
            Some("okx:fills:recorded"),
            Some("recorded"),
        );
        TrackMutationStore::insert_track_pnl_record(&*repository, &track_id, &recorded)
            .await
            .unwrap();
        let missing = audit_trade_record(
            instrument.clone(),
            Some("okx:fills:missing"),
            Some("missing"),
        );
        let unkeyed = audit_trade_record(instrument, None, Some("unkeyed"));
        let account = Arc::new(FakeRecentFillsAccount {
            response: FakeRecentFillsResponse::Available(vec![recorded, missing, unkeyed]),
        });
        let state = build_test_state_with_recent_fills_account(repository, account).await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/debug/tracks/btc-core/recent-fills-audit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: RecentFillsAuditResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.track_id, "btc-core");
        assert_eq!(payload.instrument.symbol, "BTCUSDT");
        assert_eq!(payload.availability, RecentPnlAvailabilityView::Available);
        assert_eq!(payload.completeness, RecentPnlCompletenessView::Complete);
        assert_eq!(
            payload.covered_window,
            Some(payload.required_window.clone())
        );
        assert_eq!(payload.records_seen, 3);
        assert_eq!(payload.records_recorded, 1);
        assert_eq!(payload.records_missing, 1);
        assert_eq!(payload.records_unkeyed, 1);
        assert_eq!(payload.items[0].coverage, RecentFillCoverageView::Recorded);
        assert_eq!(payload.items[1].coverage, RecentFillCoverageView::Missing);
        assert_eq!(payload.items[2].coverage, RecentFillCoverageView::Unkeyed);
        assert_eq!(
            payload.items[1].source_key.as_deref(),
            Some("okx:fills:missing")
        );
    }

    #[tokio::test]
    async fn get_recent_fills_audit_reports_unsupported_as_a_successful_capability_answer() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let state = build_test_state_with_recent_fills_account(
            repository,
            Arc::new(FakeRecentFillsAccount {
                response: FakeRecentFillsResponse::Unsupported,
            }),
        )
        .await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/debug/tracks/btc-core/recent-fills-audit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["availability"], "unsupported");
        assert_eq!(payload["completeness"], "incomplete");
        assert_eq!(payload["covered_window"], serde_json::Value::Null);
        assert_eq!(payload["records_seen"], 0);
    }

    #[tokio::test]
    async fn get_recent_fills_audit_reports_transient_query_errors_as_service_unavailable() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let state = build_test_state_with_recent_fills_account(
            repository,
            Arc::new(FakeRecentFillsAccount {
                response: FakeRecentFillsResponse::Error("temporary exchange outage".to_string()),
            }),
        )
        .await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/debug/tracks/btc-core/recent-fills-audit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["error"],
            "recent exchange PNL query failed: temporary exchange outage"
        );
    }

    #[tokio::test]
    async fn get_recent_fills_audit_reports_local_query_errors_as_internal_errors() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        let account = Arc::new(FakeRecentFillsAccount {
            response: FakeRecentFillsResponse::Available(vec![audit_trade_record(
                instrument,
                Some("binance:fills:one"),
                Some("one"),
            )]),
        });
        let mut state =
            build_test_state_with_recent_fills_account(repository, account.clone()).await;
        state.http_state.recent_fills_auditor = Arc::new(RecentFillsAuditor::new(
            account,
            Arc::new(FailingQueryStore),
        ));

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/debug/tracks/btc-core/recent-fills-audit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["error"],
            "local PNL coverage query failed: query unavailable"
        );
    }

    fn audit_trade_record(
        instrument: Instrument,
        source_key: Option<&str>,
        trade_id: Option<&str>,
    ) -> TrackPnlRecord {
        TrackPnlRecord::trade(
            instrument,
            Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 3).unwrap(),
            "okx:fills".to_string(),
            source_key.map(str::to_string),
            Some("order-1".to_string()),
            trade_id.map(str::to_string),
            Side::Sell,
            62_000.0,
            poise_engine::ledger::TrackPnlQuantity::base_asset(
                poise_core::instrument::PositionQty::new(0.2).unwrap(),
                poise_core::instrument::Asset::try_new("BTC").unwrap(),
            ),
            amount(0.0001, "USDT"),
            Some(amount(0.00001, "USDT")),
        )
    }

    struct FakeRecentFillsAccount {
        response: FakeRecentFillsResponse,
    }

    enum FakeRecentFillsResponse {
        Available(Vec<TrackPnlRecord>),
        Unsupported,
        Error(String),
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeRecentFillsAccount {
        async fn get_recent_track_pnl_records(
            &self,
            _instrument: &Instrument,
            required_window: &poise_application::exchange::PnlHistoryWindow,
        ) -> anyhow::Result<RecentTrackPnlRecords> {
            match &self.response {
                FakeRecentFillsResponse::Available(records) => {
                    Ok(RecentTrackPnlRecords::Available(
                        poise_application::exchange::RecentTrackPnlBatch::complete(
                            required_window.clone(),
                            records.clone(),
                        ),
                    ))
                }
                FakeRecentFillsResponse::Unsupported => Ok(RecentTrackPnlRecords::Unsupported),
                FakeRecentFillsResponse::Error(message) => anyhow::bail!(message.clone()),
            }
        }

        async fn subscribe_user_data(&self) -> anyhow::Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    #[derive(Default)]
    struct FailingRepository {
        updated_at: std::sync::Mutex<std::collections::HashMap<String, chrono::DateTime<Utc>>>,
    }

    impl FailingRepository {
        fn seed_track(&self, track_id: &TrackId) {
            self.updated_at
                .lock()
                .unwrap()
                .insert(track_id.as_str().to_string(), Utc::now());
        }
    }

    #[async_trait::async_trait]
    impl TrackMutationStore for FailingRepository {
        async fn record_track_activity(
            &self,
            _activity: &poise_application::StoredTrackActivity,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn commit_track_transition(
            &self,
            _id: &str,
            _control_state: Option<&poise_application::TrackControlState>,
            _events: &[poise_core::events::DomainEvent],
        ) -> anyhow::Result<CommittedTrackWrite> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn save_track_control_state(
            &self,
            _track_id: &TrackId,
            _state: &poise_application::TrackControlState,
        ) -> anyhow::Result<()> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn insert_track_pnl_record(
            &self,
            _track_id: &TrackId,
            _record: &poise_engine::ledger::TrackPnlRecord,
        ) -> anyhow::Result<bool> {
            Err(anyhow!("persistence unavailable"))
        }
    }

    #[async_trait::async_trait]
    impl TrackEffectJournal for FailingRepository {
        async fn append_entries(&self, _entries: &[EffectJournalEntry]) -> anyhow::Result<()> {
            Err(anyhow!("persistence unavailable"))
        }

        async fn record_effect_outcomes(
            &self,
            _outcomes: &[EffectStatusUpdate],
        ) -> anyhow::Result<()> {
            Err(anyhow!("persistence unavailable"))
        }
    }

    #[async_trait::async_trait]
    impl TrackQueryStore for FailingRepository {
        async fn list_recent_track_activity(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<poise_application::StoredTrackActivity>> {
            Ok(Vec::new())
        }

        async fn load_track_updated_at(
            &self,
            track_id: &TrackId,
        ) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
            Ok(self
                .updated_at
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .copied())
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> anyhow::Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn load_track_control_state(
            &self,
            _track_id: &TrackId,
        ) -> anyhow::Result<Option<poise_application::TrackControlState>> {
            Ok(None)
        }

        async fn load_track_pnl_stats(
            &self,
            _track_id: &TrackId,
            pnl_utc_day: chrono::NaiveDate,
        ) -> anyhow::Result<poise_engine::ledger::TrackPnlStats> {
            Ok(poise_engine::ledger::TrackPnlStats {
                pnl_utc_day,
                ..poise_engine::ledger::TrackPnlStats::default()
            })
        }

        async fn list_track_pnl_source_keys(
            &self,
            _track_id: &TrackId,
            _source_keys: &[String],
        ) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }
}
