//! 网页用的 HTTP 接口（第 12 节）。
//!
//! 这一层只做三件事：把请求翻译成 [`Runtime`] 的调用、把结果序列化成 JSON、把错误
//! 映射成状态码。判定「能不能下单」「要不要重新预览」都在 `runtime` 里，不在这里。
//!
//! 私钥不经过这里：[`Runtime`] 的接口上没有私钥，响应里也就不会有。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use poise_strategy::config::StrategyConfig;
use poise_strategy::order::ClientOrderId;
use serde::{Deserialize, Serialize};
use tower_http::services::{ServeDir, ServeFile};

use crate::runtime::{
    DraftPreview, ResumeOutcome, Runtime, RuntimeError, StrategyDetail, StrategySummary,
};
use crate::store::{DraftId, StoreError, StoredDraft, StrategyId};

/// 只有接口，没有网页：测试用这个。
pub fn router(runtime: Arc<Runtime>) -> Router {
    Router::new().nest("/api", api_routes().with_state(runtime))
}

/// 接口加网页控制台的静态文件。
///
/// 接口挂在 `/api` 下，其余路径交给前端单页应用：前端自己管路由，刷新任意一个页面
/// 都要能拿到 `index.html`。
pub fn router_with_web(runtime: Arc<Runtime>, web_root: PathBuf) -> Router {
    let index = web_root.join("index.html");
    Router::new()
        .nest("/api", api_routes().with_state(runtime))
        .fallback_service(ServeDir::new(web_root).fallback(ServeFile::new(index)))
}

/// 网页用的接口路由，路径相对于 `/api`。
fn api_routes() -> Router<Arc<Runtime>> {
    Router::new()
        // `/api` 下没有的路由要回 JSON 404，不能落到网页的静态文件上：前端拿到
        // 200 加一段 HTML，只会报一句和真实原因无关的解析错误。
        .fallback(api_not_found)
        .route("/strategies", get(list_strategies))
        .route("/strategies/:id", get(strategy_detail))
        .route("/strategies/:id/pause", post(pause_strategy))
        .route("/strategies/:id/resume", post(resume_strategy))
        .route("/strategies/:id/stop", post(stop_strategy))
        .route("/strategies/:id/config", put(update_config))
        .route("/drafts", get(list_drafts).post(create_draft))
        .route(
            "/drafts/:id",
            get(load_draft).put(save_draft).delete(delete_draft),
        )
        .route("/drafts/:id/preview", post(preview_draft))
        .route("/drafts/:id/start", post(start_draft))
}

/// `/api` 下没有的路径：接口自己的 404，不走网页的静态文件。
///
/// 用 `OriginalUri` 报完整路径：路由挂载在 `/api` 下，这里的 `Uri` 是去掉前缀之后的。
async fn api_not_found(axum::extract::OriginalUri(uri): axum::extract::OriginalUri) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        kind: "not_found",
        message: format!("no such endpoint: {}", uri.path()),
    }
}

/// 策略列表。只读库里的记录和视图快照，不问交易所（第 11 节）。
async fn list_strategies(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Vec<StrategySummary>>, ApiError> {
    Ok(Json(runtime.strategies()?))
}

/// 运行详情。同样只读库：页面刷得再勤也不会多一次交易所调用。
async fn strategy_detail(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<StrategyDetail>, ApiError> {
    Ok(Json(runtime.detail(&strategy_id(id)?)?))
}

async fn pause_strategy(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<CanceledBody>, ApiError> {
    let canceled = runtime.pause(&strategy_id(id)?).await?;
    Ok(Json(CanceledBody { canceled }))
}

async fn resume_strategy(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<ResumeOutcome>, ApiError> {
    Ok(Json(runtime.resume(&strategy_id(id)?).await?))
}

async fn stop_strategy(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<CanceledBody>, ApiError> {
    let canceled = runtime.stop(&strategy_id(id)?).await?;
    Ok(Json(CanceledBody { canceled }))
}

async fn update_config(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(config): Json<StrategyConfig>,
) -> Result<Json<VersionBody>, ApiError> {
    let version = runtime.update_config(&strategy_id(id)?, config)?;
    Ok(Json(VersionBody { version }))
}

async fn list_drafts(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Vec<StoredDraft>>, ApiError> {
    Ok(Json(runtime.drafts()?))
}

async fn create_draft(
    State(runtime): State<Arc<Runtime>>,
    Json(config): Json<StrategyConfig>,
) -> Result<Json<DraftIdBody>, ApiError> {
    let draft_id = runtime.create_draft(config)?;
    Ok(Json(DraftIdBody { draft_id }))
}

async fn load_draft(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<StoredDraft>, ApiError> {
    Ok(Json(runtime.draft(&strategy_id(id)?)?))
}

async fn save_draft(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(config): Json<StrategyConfig>,
) -> Result<Json<VersionBody>, ApiError> {
    let version = runtime.save_draft(&strategy_id(id)?, config)?;
    Ok(Json(VersionBody { version }))
}

async fn delete_draft(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    runtime.delete_draft(&strategy_id(id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn preview_draft(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<DraftPreview>, ApiError> {
    Ok(Json(runtime.preview_draft(&strategy_id(id)?).await?))
}

async fn start_draft(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(body): Json<StartBody>,
) -> Result<Json<StrategyIdBody>, ApiError> {
    let strategy_id = runtime.start_draft(&strategy_id(id)?, &body.token).await?;
    Ok(Json(StrategyIdBody { strategy_id }))
}

/// 路径里的标识：空的就是不合法的请求，不用去查库。
fn strategy_id(raw: String) -> Result<StrategyId, ApiError> {
    StrategyId::try_from(raw).map_err(|error| ApiError::bad_request(error.to_string()))
}

#[derive(Deserialize)]
struct StartBody {
    token: String,
}

#[derive(Serialize)]
struct CanceledBody {
    canceled: Vec<ClientOrderId>,
}

#[derive(Serialize)]
struct VersionBody {
    version: i64,
}

#[derive(Serialize)]
struct DraftIdBody {
    draft_id: DraftId,
}

#[derive(Serialize)]
struct StrategyIdBody {
    strategy_id: StrategyId,
}

/// 接口错误：一个状态码、一个机器可读的类别、一句人能看懂的话。
///
/// 话直接来自 [`RuntimeError`] 的说明——它说的是「为什么不行、接下来该做什么」，
/// 正是网页要显示给用户的东西。类别给网页用：同样是 409，「预览过期」要引导用户
/// 重新预览，「标的有别的策略在跑」要让他去停掉那一个，网页不该去解析这句话。
pub struct ApiError {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl ApiError {
    fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "bad_request",
            message,
        }
    }
}

impl From<RuntimeError> for ApiError {
    fn from(error: RuntimeError) -> Self {
        let (status, kind) = match &error {
            RuntimeError::Store(StoreError::UnknownStrategy(_) | StoreError::UnknownDraft(_)) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            // 参数本身不行：改完再提交。
            RuntimeError::Config(_)
            | RuntimeError::ConfigChangeNotAllowed { .. }
            | RuntimeError::Store(StoreError::EmptyStrategyId) => {
                (StatusCode::BAD_REQUEST, "invalid_config")
            }
            // 现在这个状态下行不通，但状态会变：重新预览、等它停、或者换个标的。
            RuntimeError::PreviewStale(_) => (StatusCode::CONFLICT, "preview_stale"),
            RuntimeError::InstrumentBusy { .. } => (StatusCode::CONFLICT, "instrument_busy"),
            RuntimeError::NotRunning(..) => (StatusCode::CONFLICT, "not_running"),
            RuntimeError::Busy(_) => (StatusCode::CONFLICT, "busy"),
            RuntimeError::Archived(_) => (StatusCode::CONFLICT, "archived"),
            // 交易所或账户那边的问题，不是这次请求的错。
            RuntimeError::Exchange(_) => (StatusCode::BAD_GATEWAY, "exchange"),
            // 库里的状态和计划对不上：是本地的数据问题，不是交易所故障。
            RuntimeError::State(_) | RuntimeError::Store(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        Self {
            status,
            kind,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                kind: self.kind,
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct ErrorBody {
    /// 机器可读的错误类别，见 [`ApiError`] 的说明。
    kind: &'static str,
    error: String,
}
