//! 网页用的 HTTP 接口（第 12 节）。
//!
//! 全程用模拟交易所和模拟账户，只走 `Runtime`，不连任何外部服务。业务判定在
//! `runtime` 里，这一层只负责请求、响应和状态码。

mod common;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use common::{
    at, console, long_grid_config, long_side_mut, neutral_console, neutral_grid_config,
    short_grid_config, throttled,
};
use poise_runtime::api;
use poise_strategy::config::StrategyConfig;
use poise_strategy::units::{Price, Rate};
use serde_json::{Value, json};
use tower::ServiceExt;

/// 发一个请求，返回状态码和响应体。
async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let request = match body {
        Some(body) => request.body(Body::from(body.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    (status, value)
}

fn config_body(config: &StrategyConfig) -> Value {
    serde_json::to_value(config).unwrap()
}

/// 发一个请求，按原文返回响应体：静态文件不是 JSON。
async fn text(router: &Router, uri: &str) -> (StatusCode, String) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// 网页上的一整条流程：新建草稿、保存、预览、确认启动、看运行、暂停、恢复、停止。
#[tokio::test]
async fn the_console_runs_the_whole_flow_over_http() {
    let console = console();
    let router = api::router(console.runtime.clone());

    // 新建草稿。
    let (status, body) = call(
        &router,
        Method::POST,
        "/api/drafts",
        Some(config_body(&long_grid_config())),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let draft_id = body["draft_id"].as_str().unwrap().to_string();

    // 保存改过的参数。
    let edited = throttled(long_grid_config(), chrono::Duration::minutes(60));
    let (status, body) = call(
        &router,
        Method::PUT,
        &format!("/api/drafts/{draft_id}"),
        Some(config_body(&edited)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], 2);

    // 草稿列表里能看到它。
    let (status, body) = call(&router, Method::GET, "/api/drafts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["draft_id"], draft_id);

    // 预览。
    let (status, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["validation"]["valid"], true);
    assert_eq!(preview["config_version"], 2);
    assert_eq!(preview["cells"].as_array().unwrap().len(), 20);
    let token = preview["token"].as_str().unwrap().to_string();

    // 确认启动之前交易所上一张单都没有。
    assert_eq!(console.exchange.place_count(), 0);

    // 确认启动。
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/start"),
        Some(json!({ "token": token })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["strategy_id"], draft_id);
    // 草稿变成了策略。
    let (_, body) = call(&router, Method::GET, "/api/drafts", None).await;
    assert!(body.as_array().unwrap().is_empty());

    // 策略列表。
    let (status, body) = call(&router, Method::GET, "/api/strategies", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["status"], "running");
    assert_eq!(body[0]["instrument"], "BTC");
    assert_eq!(body[0]["config_version"], 2);

    // 走一步之后看运行详情。
    console.clock.set(at(1));
    console
        .runtime
        .step(&console.runtime.drivable().unwrap()[0])
        .await
        .unwrap();
    let (status, detail) = call(
        &router,
        Method::GET,
        &format!("/api/strategies/{draft_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["cells"].as_array().unwrap().len(), 20);
    assert_eq!(detail["orders"].as_array().unwrap().len(), 1);
    assert_eq!(detail["summary"]["working_orders"], 1);
    assert_eq!(detail["config_versions"].as_array().unwrap().len(), 2);

    // 暂停。
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/strategies/{draft_id}/pause"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["canceled"].as_array().unwrap().len(), 1);

    // 恢复。
    console.clock.set(at(10));
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/strategies/{draft_id}/resume"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["Resumed"]["status"], "running");

    // 停止。
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/strategies/{draft_id}/stop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, detail) = call(
        &router,
        Method::GET,
        &format!("/api/strategies/{draft_id}"),
        None,
    )
    .await;
    assert_eq!(detail["summary"]["status"], "stopped");
    assert!(detail["orders"].as_array().unwrap().is_empty());
    assert!(
        console
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working())
    );
}

/// B43：预览之后行情变了，旧令牌确认不了，接口回答 409。
#[tokio::test]
async fn b43_a_stale_token_is_a_conflict() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let draft_id = create_draft(&router, &long_grid_config()).await;
    let (_, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    let token = preview["token"].as_str().unwrap().to_string();

    console
        .exchange
        .set_market_price(Price::parse("94999").unwrap(), at(10));
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/start"),
        Some(json!({ "token": token })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("preview again"),
        "{body}"
    );

    // 重新预览之后能启动。
    let (_, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    let token = preview["token"].as_str().unwrap().to_string();
    let (status, _) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/start"),
        Some(json!({ "token": token })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// B46：同一个标的的第二个策略启动不了，接口回答 409。
#[tokio::test]
async fn b46_a_busy_instrument_is_a_conflict() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let first = create_draft(&router, &long_grid_config()).await;
    start(&router, &first).await;

    let second = create_draft(&router, &long_grid_config()).await;
    let (_, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{second}/preview"),
        None,
    )
    .await;
    let token = preview["token"].as_str().unwrap().to_string();
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{second}/start"),
        Some(json!({ "token": token })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains(&first),
        "the reason names the strategy that holds the instrument: {body}"
    );
}

/// 暂停、停止的接口报出策略真正落到的状态，页面照着它说话。
///
/// 撤单回执丢在路上的时候，「暂停中」「停止中」是真实存在的状态：订单还在交易所上，
/// 后台每一轮继续撤。接口只回一个撤销列表的话，页面只能一律说「已暂停」「已停止」，
/// 用户以为挂单已经撤掉了（第 2.4 节）。
#[tokio::test]
async fn a_pause_or_stop_reports_the_status_it_actually_reached() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let strategy_id = create_draft(&router, &long_grid_config()).await;
    start(&router, &strategy_id).await;
    // 挂上单再暂停。
    console.clock.set(at(1));
    console
        .runtime
        .step(&console.runtime.drivable().unwrap()[0])
        .await
        .unwrap();

    // 撤单回执丢了：交易所那边订单还在，状态停在「暂停中」。
    console.exchange.unknown_next_cancel("timed out");
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/strategies/{strategy_id}/pause"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "pausing");
    assert_eq!(body["canceled"].as_array().unwrap().len(), 19);

    // 再点一次：这一轮撤得掉，才转「已暂停」。
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/strategies/{strategy_id}/pause"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "paused");
    assert_eq!(body["canceled"].as_array().unwrap().len(), 1);

    // 停止也一样：先「停止中」，收尾之后才是「已停止」。先恢复起来，好有挂单可撤。
    console.clock.set(at(10));
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/strategies/{strategy_id}/resume"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    console.exchange.unknown_next_cancel("timed out");
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/strategies/{strategy_id}/stop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "stopping");
    assert_eq!(body["canceled"].as_array().unwrap().len(), 19);

    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/strategies/{strategy_id}/stop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "stopped");
    assert_eq!(body["canceled"].as_array().unwrap().len(), 1);
}

/// 归档的策略不能再驱动，接口回答 409。
#[tokio::test]
async fn an_archived_strategy_refuses_further_commands() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let strategy_id = create_draft(&router, &long_grid_config()).await;
    start(&router, &strategy_id).await;
    call(
        &router,
        Method::POST,
        &format!("/api/strategies/{strategy_id}/stop"),
        None,
    )
    .await;

    for action in ["pause", "resume"] {
        let (status, body) = call(
            &router,
            Method::POST,
            &format!("/api/strategies/{strategy_id}/{action}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}: {body}");
    }
    let (status, body) = call(
        &router,
        Method::PUT,
        &format!("/api/strategies/{strategy_id}/config"),
        Some(config_body(&long_grid_config())),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

/// 运行中只能改限速间隔；改别的参数是 400，并说明是哪个字段。
#[tokio::test]
async fn b44_changing_a_locked_parameter_while_running_is_a_bad_request() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let strategy_id = create_draft(&router, &long_grid_config()).await;
    start(&router, &strategy_id).await;

    let mut moved = long_grid_config();
    common::long_side_mut(&mut moved).grid_spacing = Price::parse("500").unwrap();
    let (status, body) = call(
        &router,
        Method::PUT,
        &format!("/api/strategies/{strategy_id}/config"),
        Some(config_body(&moved)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("grid_spacing"),
        "{body}"
    );

    // 限速间隔可以改。
    let (status, body) = call(
        &router,
        Method::PUT,
        &format!("/api/strategies/{strategy_id}/config"),
        Some(config_body(&throttled(
            long_grid_config(),
            chrono::Duration::minutes(30),
        ))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], 2);
}

/// 校验不过的配置：草稿存得下（参数要等预览时按当时的合约规则校验），
/// 预览给出原因，确认启动是 400。
#[tokio::test]
async fn an_invalid_config_cannot_be_started() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let mut config = long_grid_config();
    common::long_side_mut(&mut config).grid_spread = Price::parse("1").unwrap();
    let draft_id = create_draft(&router, &config).await;

    let (status, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["validation"]["valid"], false);
    assert!(
        preview["validation"]["reason"]
            .as_str()
            .unwrap()
            .contains("spread"),
        "{preview}"
    );
    assert!(preview["token"].is_null());

    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/start"),
        Some(json!({ "token": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("spread"), "{body}");
}

/// 做空网格：只配空头侧的配置从草稿一路走到运行详情，表里都是空头侧。
#[tokio::test]
async fn a_short_grid_can_be_created_previewed_and_started() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let draft_id = create_draft(&router, &short_grid_config()).await;

    // 存下来的是「只有空头侧」：多头侧是 null，不是一份空配置。
    let (status, draft) = call(
        &router,
        Method::GET,
        &format!("/api/drafts/{draft_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert!(draft["config"]["long"].is_null(), "{draft}");
    assert_eq!(draft["config"]["short"]["price_low"], "75000", "{draft}");

    let (status, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["validation"]["valid"], true, "{preview}");
    let cells = preview["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 20);
    assert!(
        cells.iter().all(|cell| cell["direction"] == "short"),
        "{preview}"
    );
    // 空头侧的格子从最低价往上：第 0 格开空价 75000，买平价 74000。
    assert_eq!(cells[0]["direction"], "short", "{preview}");
    assert_eq!(cells[0]["entry_price"], "75000", "{preview}");
    assert_eq!(cells[0]["exit_price"], "74000", "{preview}");
    assert!(cells[0]["batch"].is_null(), "{preview}");
    // r = 0：没有主仓，批次表是空的。
    assert!(
        preview["batches"].as_array().unwrap().is_empty(),
        "{preview}"
    );

    let token = preview["token"].as_str().unwrap().to_string();
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/start"),
        Some(json!({ "token": token })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 列表和运行详情也按空头侧报。
    let (_, list) = call(&router, Method::GET, "/api/strategies", None).await;
    assert!(list[0]["config"]["long"].is_null(), "{list}");
    assert_eq!(list[0]["config"]["short"]["price_low"], "75000", "{list}");
    let (status, detail) = call(
        &router,
        Method::GET,
        &format!("/api/strategies/{draft_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["cells"].as_array().unwrap().len(), 20);
    assert_eq!(detail["cells"][0]["direction"], "short", "{detail}");
}

/// 中性网格：两侧都配的配置，两侧的格子都在，两侧都没有主仓。
#[tokio::test]
async fn a_neutral_grid_can_be_previewed_and_started() {
    // 设计里的价格与数量在 10 美元最低金额下过不了校验，用最低金额 1 美元的合约规则。
    let console = neutral_console();
    let router = api::router(console.runtime.clone());
    let draft_id = create_draft(&router, &neutral_grid_config()).await;

    let (status, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["validation"]["valid"], true, "{preview}");
    // 两侧都没有主仓，批次表是空的。
    assert!(
        preview["batches"].as_array().unwrap().is_empty(),
        "{preview}"
    );
    // 多头侧十格（89 往下到 80）在前，空头侧五格（91 往上到 95）在后。
    let cells = preview["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 15);
    assert_eq!(cells[0]["direction"], "long", "{preview}");
    assert_eq!(cells[0]["entry_price"], "89", "{preview}");
    assert_eq!(cells[9]["direction"], "long", "{preview}");
    assert_eq!(cells[9]["entry_price"], "80", "{preview}");
    assert_eq!(cells[10]["direction"], "short", "{preview}");
    assert_eq!(cells[10]["entry_price"], "91", "{preview}");
    assert_eq!(cells[14]["entry_price"], "95", "{preview}");

    let token = preview["token"].as_str().unwrap().to_string();
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/start"),
        Some(json!({ "token": token })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, detail) = call(
        &router,
        Method::GET,
        &format!("/api/strategies/{draft_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["cells"].as_array().unwrap().len(), 15);
    let config = &detail["summary"]["config"];
    assert_eq!(config["long"]["price_low"], "79", "{detail}");
    assert_eq!(config["short"]["price_low"], "91", "{detail}");
}

/// 两侧之间的校验在接口上照原样说出来：区间重叠、中性网格带主仓，预览都不通过。
#[tokio::test]
async fn a_two_sided_config_error_is_reported_by_the_preview() {
    let console = neutral_console();
    let router = api::router(console.runtime.clone());

    // 多头侧的上限 91 不低于空头侧的下限 91：两侧的建仓区间重叠。
    let mut overlapping = neutral_grid_config();
    long_side_mut(&mut overlapping).price_high = Price::parse("91").unwrap();
    let draft_id = create_draft(&router, &overlapping).await;
    let (status, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["validation"]["valid"], false, "{preview}");
    assert!(
        preview["validation"]["reason"]
            .as_str()
            .unwrap()
            .contains("below the short side"),
        "{preview}"
    );
    // 没有令牌：确认启动无从谈起。
    assert!(preview["token"].is_null(), "{preview}");
    let (status, body) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/start"),
        Some(json!({ "token": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("below the short side"),
        "{body}"
    );

    // 中性网格两侧都没有主仓：给多头侧设主仓是 400，原因也照原样说出来。
    let mut with_core = neutral_grid_config();
    let long = long_side_mut(&mut with_core);
    long.core_ratio = Rate::parse("0.5").unwrap();
    long.batch_count = Some(1);
    let draft_id = create_draft(&router, &with_core).await;
    let (status, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["validation"]["valid"], false, "{preview}");
    assert!(
        preview["validation"]["reason"]
            .as_str()
            .unwrap()
            .contains("neutral grid has no core"),
        "{preview}"
    );
}

/// 不存在的策略和草稿都是 404。
#[tokio::test]
async fn unknown_ids_are_not_found() {
    let console = console();
    let router = api::router(console.runtime.clone());
    for uri in ["/api/strategies/s-missing", "/api/drafts/s-missing"] {
        let (status, body) = call(&router, Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }
    let (status, _) = call(&router, Method::POST, "/api/drafts/s-missing/preview", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// 账户风控设置：读到默认值，改了之后读回来是新的；不合理的设置回 400 并说明原因。
#[tokio::test]
async fn the_account_settings_can_be_read_and_changed() {
    let console = console();
    let router = api::router(console.runtime.clone());

    let (status, body) = call(&router, Method::GET, "/api/account/settings", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"margin_alert_ratio": "0.3", "margin_stop_ratio": "0.5", "market_data_max_age": 10})
    );

    let (status, body) = call(
        &router,
        Method::PUT,
        "/api/account/settings",
        Some(json!({"margin_alert_ratio": "0.25", "margin_stop_ratio": "0.4", "market_data_max_age": 15})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = call(&router, Method::GET, "/api/account/settings", None).await;
    assert_eq!(
        body,
        json!({"margin_alert_ratio": "0.25", "margin_stop_ratio": "0.4", "market_data_max_age": 15})
    );

    let (status, body) = call(
        &router,
        Method::PUT,
        "/api/account/settings",
        Some(json!({"margin_alert_ratio": "0.6", "margin_stop_ratio": "0.5", "market_data_max_age": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["kind"], "invalid_config");
    let (_, body) = call(&router, Method::GET, "/api/account/settings", None).await;
    assert_eq!(body["margin_stop_ratio"], "0.4", "被拒的设置没有保存");
}

/// 删草稿：删掉之后就没有了，再删一次也不报错。
#[tokio::test]
async fn a_draft_can_be_deleted() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let draft_id = create_draft(&router, &long_grid_config()).await;

    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/api/drafts/{draft_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &router,
        Method::GET,
        &format!("/api/drafts/{draft_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/api/drafts/{draft_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// 走完一整条流程，任何一个响应里都不出现私钥或账户凭据。
#[tokio::test]
async fn no_response_carries_credentials() {
    let console = console();
    let router = api::router(console.runtime.clone());
    let draft_id = create_draft(&router, &long_grid_config()).await;
    let (_, preview) = call(
        &router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    let token = preview["token"].as_str().unwrap().to_string();

    let bodies = vec![
        call(&router, Method::GET, "/api/drafts", None).await.1,
        call(
            &router,
            Method::GET,
            &format!("/api/drafts/{draft_id}"),
            None,
        )
        .await
        .1,
        preview.clone(),
        call(
            &router,
            Method::POST,
            &format!("/api/drafts/{draft_id}/start"),
            Some(json!({ "token": token })),
        )
        .await
        .1,
        call(&router, Method::GET, "/api/strategies", None).await.1,
        call(
            &router,
            Method::GET,
            &format!("/api/strategies/{draft_id}"),
            None,
        )
        .await
        .1,
    ];

    for body in bodies {
        let text = body.to_string().to_lowercase();
        for forbidden in ["private", "secret", "api_wallet", "signature"] {
            assert!(
                !text.contains(forbidden),
                "the response mentions `{forbidden}`: {body}"
            );
        }
    }
}

/// 网页和接口在同一个端口上：页面路径都交给前端，`/api` 下不存在的路径仍然是 JSON 错误。
#[tokio::test]
async fn the_console_serves_the_web_page_from_the_same_port() {
    let console = console();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("index.html"), "<!doctype html>控制台").unwrap();
    std::fs::create_dir(directory.path().join("assets")).unwrap();
    std::fs::write(directory.path().join("assets/app.js"), "console.log(1)").unwrap();
    let router = api::router_with_web(console.runtime.clone(), directory.path().to_path_buf());

    let (status, page) = text(&router, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("控制台"), "{page}");

    let (status, script) = text(&router, "/assets/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(script.contains("console.log"), "{script}");

    // 前端自己管路由：刷新一个页面路径也要拿到首页，而不是 404。
    let (status, page) = text(&router, "/strategies/s-1").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("控制台"), "{page}");

    // 接口还是接口：不存在的策略给 404 JSON，不是首页。
    let (status, body) = call(&router, Method::GET, "/api/strategies/s-missing", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // 打错的接口路径也是 JSON 404：前端拿到首页会报一句和真实原因无关的解析错误。
    let (status, body) = call(&router, Method::GET, "/api/strategy/s-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["kind"], "not_found", "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("/api/strategy"),
        "{body}"
    );
}

async fn create_draft(router: &Router, config: &StrategyConfig) -> String {
    let (status, body) = call(
        router,
        Method::POST,
        "/api/drafts",
        Some(config_body(config)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["draft_id"].as_str().unwrap().to_string()
}

async fn start(router: &Router, draft_id: &str) {
    let (status, preview) = call(
        router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/preview"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let token = preview["token"].as_str().unwrap().to_string();
    let (status, body) = call(
        router,
        Method::POST,
        &format!("/api/drafts/{draft_id}/start"),
        Some(json!({ "token": token })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
