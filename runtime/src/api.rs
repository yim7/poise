//! 网页用的 HTTP 接口（第 12 节）。
//!
//! 这一层只做三件事：把请求翻译成 [`Runtime`] 的调用、把结果序列化成 JSON、把错误
//! 映射成状态码。判定「能不能下单」「要不要重新预览」都在 `runtime` 里，不在这里。
//!
//! 私钥不经过这里：[`Runtime`] 的接口上没有私钥，响应里也就不会有。

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use poise_strategy::config::StrategyConfig;
use poise_strategy::order::ClientOrderId;
use serde::{Deserialize, Serialize};
use tower_http::services::{ServeDir, ServeFile};

use crate::runtime::{
    DraftPreview, InstanceInfo, ResumeOutcome, Runtime, RuntimeError, StrategyDetail,
    StrategySummary,
};
use crate::settings::AccountSettings;
use crate::store::{DraftId, StoreError, StoredDraft, StrategyId, StrategyStatus};

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
        .route("/instance", get(instance))
        .route("/strategies", get(list_strategies))
        .route("/strategies/:id", get(strategy_detail))
        .route("/strategies/:id/pause", post(pause_strategy))
        .route("/strategies/:id/resume", post(resume_strategy))
        .route("/strategies/:id/stop", post(stop_strategy))
        .route("/strategies/:id/close", post(close_strategy))
        .route("/strategies/:id/config", put(update_config))
        .route("/drafts", get(list_drafts).post(create_draft))
        .route(
            "/drafts/:id",
            get(load_draft).put(save_draft).delete(delete_draft),
        )
        .route("/drafts/:id/preview", post(preview_draft))
        .route("/drafts/:id/start", post(start_draft))
        .route(
            "/account/settings",
            get(account_settings).put(update_account_settings),
        )
        .layer(middleware::from_fn(require_local_browser))
}

/// 控制台没有登录，只靠「只能从本机访问」。这里再核对一次请求是不是真的来自本机页面。
///
/// 只监听 127.0.0.1 挡不住两件事：无请求体的 POST（暂停、恢复、停止、预览）是 CORS
/// 「简单请求」，用户正在看的任何网页都能跨站发给本机端口；DNS 重绑定还能让浏览器带着
/// 别人的域名当 `Host` 访问本机。浏览器一定会带 `Host`，跨站时还会带 `Origin`：两个头
/// 任何一个不是本机地址就拒绝。没有这两个头的请求（命令行、测试）不是浏览器，照常放行。
async fn require_local_browser(request: Request, next: Next) -> Response {
    for (name, with_scheme) in [("host", false), ("origin", true)] {
        let Some(value) = request.headers().get(name) else {
            continue;
        };
        let authority = value.to_str().ok().and_then(|value| {
            if with_scheme {
                value
                    .strip_prefix("http://")
                    .or_else(|| value.strip_prefix("https://"))
            } else {
                Some(value)
            }
        });
        if !authority.is_some_and(is_local_authority) {
            return ApiError {
                status: StatusCode::FORBIDDEN,
                kind: "not_local",
                message: format!(
                    "the `{name}` header is not a local address; the console only answers pages served from this machine"
                ),
            }
            .into_response();
        }
    }
    next.run(request).await
}

/// `host[:port]` 是不是本机：`localhost`、`127.x.x.x` 或 `[::1]`。
fn is_local_authority(authority: &str) -> bool {
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map(|(host, _)| host),
        None => Some(
            authority
                .rsplit_once(':')
                .map_or(authority, |(host, _)| host),
        ),
    };
    let Some(host) = host else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
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

/// 这个实例连的网络和账户：页面顶部的标识。
async fn instance(State(runtime): State<Arc<Runtime>>) -> Result<Json<InstanceInfo>, ApiError> {
    Ok(Json(runtime.instance()?))
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
    let strategy_id = strategy_id(id)?;
    let canceled = runtime.pause(&strategy_id).await?;
    let status = runtime.status(&strategy_id)?;
    Ok(Json(CanceledBody { canceled, status }))
}

async fn resume_strategy(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<ResumeOutcome>, ApiError> {
    Ok(Json(runtime.resume(&strategy_id(id)?).await?))
}

/// 停止的选项：`close=true` 表示撤单收尾之后市价平仓（第 7.7 节）。不带就是普通停止。
#[derive(Deserialize)]
struct StopQuery {
    #[serde(default)]
    close: bool,
}

async fn stop_strategy(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Query(query): Query<StopQuery>,
) -> Result<Json<CanceledBody>, ApiError> {
    let strategy_id = strategy_id(id)?;
    let canceled = if query.close {
        runtime.stop_and_close(&strategy_id).await?
    } else {
        runtime.stop(&strategy_id).await?
    };
    let status = runtime.status(&strategy_id)?;
    Ok(Json(CanceledBody { canceled, status }))
}

#[derive(Serialize)]
struct StatusBody {
    status: StrategyStatus,
}

/// 市价平仓：停止中的补上平仓标记，已归档的进入平仓中（第 7.7 节）。
async fn close_strategy(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
) -> Result<Json<StatusBody>, ApiError> {
    let strategy_id = strategy_id(id)?;
    let status = runtime.close(&strategy_id).await?;
    Ok(Json(StatusBody { status }))
}

async fn update_config(
    State(runtime): State<Arc<Runtime>>,
    Path(id): Path<String>,
    Json(config): Json<StrategyConfig>,
) -> Result<Json<VersionBody>, ApiError> {
    let version = runtime.update_config(&strategy_id(id)?, config)?;
    Ok(Json(VersionBody { version }))
}

/// 账户风控设置：保证金率的提醒阈值、停止阈值和行情过期时间（第 11 节）。
async fn account_settings(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<AccountSettings>, ApiError> {
    Ok(Json(runtime.account_settings()?))
}

/// 修改账户风控设置。不合理的设置回 400，原样说明原因。
async fn update_account_settings(
    State(runtime): State<Arc<Runtime>>,
    Json(settings): Json<AccountSettings>,
) -> Result<Json<AccountSettings>, ApiError> {
    runtime.update_account_settings(settings)?;
    Ok(Json(runtime.account_settings()?))
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

/// 暂停、停止的结果：撤销了哪些单，以及策略最后落在哪个状态。
///
/// 带上状态是因为撤单不一定收得了尾：撤单回执丢了、成交还没对完账时，策略停在
/// 「暂停中」「停止中」，后台每一轮继续撤。只回一个撤销列表的话，页面只能一律说
/// 「已暂停」「已停止」，用户以为挂单已经撤掉了。
#[derive(Serialize)]
struct CanceledBody {
    canceled: Vec<ClientOrderId>,
    status: StrategyStatus,
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
            | RuntimeError::Settings(_)
            | RuntimeError::ConfigChangeNotAllowed { .. }
            | RuntimeError::Store(StoreError::EmptyStrategyId) => {
                (StatusCode::BAD_REQUEST, "invalid_config")
            }
            // 现在这个状态下行不通，但状态会变：重新预览、等它停、或者换个标的。
            RuntimeError::PreviewStale(_) => (StatusCode::CONFLICT, "preview_stale"),
            RuntimeError::InstrumentBusy { .. } => (StatusCode::CONFLICT, "instrument_busy"),
            // 标的上的仓位接管不了：用户先处理仓位，或者改参数。
            RuntimeError::Takeover(_) => (StatusCode::CONFLICT, "takeover_refused"),
            RuntimeError::NotRunning(..) => (StatusCode::CONFLICT, "not_running"),
            RuntimeError::Busy(_) => (StatusCode::CONFLICT, "busy"),
            RuntimeError::Archived(_) => (StatusCode::CONFLICT, "archived"),
            RuntimeError::NothingToClose(_) => (StatusCode::CONFLICT, "nothing_to_close"),
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

#[cfg(test)]
mod tests {
    use super::is_local_authority;

    /// 浏览器会带的本机写法都认：带不带端口、IPv4、IPv6、`localhost`。
    #[test]
    fn local_authorities_are_recognised() {
        for authority in [
            "127.0.0.1:8010",
            "127.0.0.1",
            "127.1.2.3:8000",
            "localhost:5173",
            "LocalHost",
            "[::1]:8010",
            "[::1]",
        ] {
            assert!(is_local_authority(authority), "{authority}");
        }
    }

    /// 别人的域名、局域网地址、空值都不是本机，哪怕它解析到 127.0.0.1（DNS 重绑定）。
    #[test]
    fn other_authorities_are_refused() {
        for authority in [
            "evil.example:8010",
            "192.168.1.10:8010",
            "0.0.0.0:8010",
            "localhost.evil.example",
            "[::2]:8010",
            "",
            "null",
        ] {
            assert!(!is_local_authority(authority), "{authority}");
        }
    }
}
