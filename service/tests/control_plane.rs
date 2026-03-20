use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{Router, body::Body, http::Request};
use futures_util::StreamExt;
use grid_platform_service::{
    Application, build_app,
    integrations::binance::{
        BinanceConfig, BinanceTransport, ExchangeSymbol, MarketStreamEvent, TradingSchedule,
        TradingSession,
    },
    protocol::{
        CommandAccepted, CommandAck, CommandLinks, CommandRecord, CommandRequest, CommandStatus,
        CommandType, HttpSuccessEnvelope, OpenOrdersSource, RiskEvent, RiskLevel, RuntimeSnapshot,
        ServerEnvelope, ServerEvent, SystemEvent,
    },
    storage::{PersistedRuntime, SqliteStorage},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tokio::{
    net::TcpListener,
    sync::Mutex,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::connect_async;
use tower::ServiceExt;

#[derive(Clone)]
struct ScriptedBinanceTransport {
    exchange_info: ExchangeSymbol,
    trading_schedule: TradingSchedule,
    market_streams: Arc<Mutex<VecDeque<tokio::sync::mpsc::Receiver<MarketStreamEvent>>>>,
}

impl ScriptedBinanceTransport {
    fn new(exchange_info: ExchangeSymbol, trading_schedule: TradingSchedule) -> Self {
        Self {
            exchange_info,
            trading_schedule,
            market_streams: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    async fn push_market_stream(&self, receiver: tokio::sync::mpsc::Receiver<MarketStreamEvent>) {
        self.market_streams.lock().await.push_back(receiver);
    }
}

#[async_trait]
impl BinanceTransport for ScriptedBinanceTransport {
    async fn fetch_exchange_info(&self, symbol: &str) -> anyhow::Result<ExchangeSymbol> {
        if self.exchange_info.symbol != symbol {
            anyhow::bail!("unexpected symbol {symbol}");
        }
        Ok(self.exchange_info.clone())
    }

    async fn fetch_trading_schedule(&self) -> anyhow::Result<TradingSchedule> {
        Ok(self.trading_schedule.clone())
    }

    async fn connect_market_stream(
        &self,
        _symbol: &str,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<MarketStreamEvent>> {
        self.market_streams
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no scripted market stream available"))
    }

    async fn create_user_stream(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    async fn connect_user_stream(
        &self,
        _listen_key: &str,
    ) -> anyhow::Result<
        tokio::sync::mpsc::Receiver<grid_platform_service::integrations::binance::UserStreamEvent>,
    > {
        anyhow::bail!("user stream not expected in this test")
    }

    async fn keepalive_user_stream(&self, _listen_key: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_routes_query_snapshot_and_command_via_application() -> Result<()> {
    let app = build_app(Application::bootstrap());

    let snapshot = decode_json::<HttpSuccessEnvelope<RuntimeSnapshot>>(
        app.clone(),
        Request::builder()
            .uri("/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(snapshot.data.runtime.symbol, "XAUUSDT");

    let accepted = decode_json::<HttpSuccessEnvelope<CommandAccepted>>(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/commands/pause")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"command_id":"cmd_pause_route"}"#))
            .expect("request"),
    )
    .await?;
    assert_eq!(accepted.data.command, CommandType::Pause);

    let refreshed = decode_json::<HttpSuccessEnvelope<RuntimeSnapshot>>(
        app,
        Request::builder()
            .uri("/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(refreshed.data.runtime.strategy_state, "paused");
    assert!(
        refreshed
            .data
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| ack.command_id == "cmd_pause_route")
    );
    assert_eq!(
        refreshed.data.execution.recent_commands[0].command_id,
        "cmd_pause_route"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_snapshot_payload_exposes_open_orders_source() -> Result<()> {
    let app = build_app(Application::bootstrap());

    let snapshot = decode_json::<Value>(
        app,
        Request::builder()
            .uri("/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    assert_eq!(
        snapshot["data"]["execution"]["open_orders_source"],
        "strategy_mirror"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_snapshot_payload_normalizes_open_orders_source_for_binance_bootstrap() -> Result<()>
{
    let now_ms = chrono::Utc::now().timestamp_millis();
    let transport = ScriptedBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: std::collections::HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    );
    let (market_tx, market_rx) = tokio::sync::mpsc::channel(1);
    transport.push_market_stream(market_rx).await;
    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.metadata_refresh_interval = Duration::from_secs(3600);
    config.health_tick_interval = Duration::from_secs(3600);
    config.reconnect_base_delay = Duration::from_secs(3600);
    config.reconnect_max_delay = Duration::from_secs(3600);

    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot.execution.open_orders_source = OpenOrdersSource::ExchangeLive;

    let app = Application::bootstrap_with_runtime_and_binance(runtime, config, Arc::new(transport));
    drop(market_tx);

    assert_eq!(
        app.snapshot().execution.open_orders_source,
        OpenOrdersSource::StrategyMirror
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_streams_initial_snapshot_and_command_ack() -> Result<()> {
    let server = TestServer::spawn().await?;
    let (mut ws, _) = connect_async(&server.ws_url)
        .await
        .context("failed to connect websocket")?;

    let initial = next_event(&mut ws).await?;
    assert!(initial.sequence.is_some());
    match initial.event {
        ServerEvent::RuntimeSnapshot(snapshot) => {
            assert_eq!(snapshot.runtime.symbol, "XAUUSDT");
        }
        other => panic!("unexpected initial event: {other:?}"),
    }

    server
        .http
        .post(format!("{}/commands/pause", server.base_url))
        .json(&CommandRequest {
            command_id: "cmd_pause_ws".into(),
        })
        .send()
        .await
        .context("failed to send pause command")?
        .error_for_status()
        .context("pause command returned non-success")?;

    let ack_event = next_event(&mut ws).await?;
    assert!(ack_event.sequence.is_some());
    match ack_event.event {
        ServerEvent::CommandAck(CommandAck {
            command_id,
            command,
            ..
        }) => {
            assert_eq!(command_id, "cmd_pause_ws");
            assert_eq!(command, CommandType::Pause);
        }
        other => panic!("unexpected websocket event: {other:?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_sequence_covers_buffered_events_before_initial_snapshot() -> Result<()> {
    let application = Application::bootstrap();
    let mut receiver = application.subscribe_events();

    application
        .submit_command(
            CommandType::Pause,
            CommandRequest {
                command_id: "cmd_pause_snapshot_seq".into(),
            },
        )
        .await?;

    let snapshot = application.runtime_snapshot_event();
    let event = timeout(Duration::from_secs(1), receiver.recv())
        .await
        .context("timed out waiting for buffered event")?
        .context("broadcast channel closed unexpectedly")?;

    assert!(
        event
            .sequence
            .zip(snapshot.sequence)
            .is_some_and(|(event_sequence, snapshot_sequence)| event_sequence <= snapshot_sequence)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_query_routes_support_runtime_and_list_filters() -> Result<()> {
    let app = build_app(Application::bootstrap());

    let runtime = decode_json::<Value>(
        app.clone(),
        Request::builder()
            .uri("/query/runtime")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(runtime["data"]["instance_id"], "local");
    assert_eq!(runtime["data"]["snapshot"]["runtime"]["symbol"], "XAUUSDT");

    let orders = decode_json::<Value>(
        app.clone(),
        Request::builder()
            .uri("/query/orders?side=sell&page=1&per_page=1")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(orders["data"]["instance_id"], "local");
    assert_eq!(orders["data"]["filters"]["side"], "sell");
    assert_eq!(orders["data"]["pagination"]["page"], 1);
    assert_eq!(orders["data"]["pagination"]["per_page"], 1);
    assert_eq!(orders["data"]["pagination"]["total_items"], 1);
    assert_eq!(orders["data"]["items"][0]["side"], "sell");

    let fills = decode_json::<Value>(
        app,
        Request::builder()
            .uri("/query/fills?client_order_id=flatten_reduce_only_01")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(
        fills["data"]["filters"]["client_order_id"],
        "flatten_reduce_only_01"
    );
    assert_eq!(fills["data"]["items"][0]["trade_id"], "fill_9001");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_query_routes_sort_and_filter_commands_and_alerts() -> Result<()> {
    let (_temp_dir, app) = app_with_persisted_runtime(seed_query_runtime())?;

    let commands = decode_json::<Value>(
        app.clone(),
        Request::builder()
            .uri("/query/commands?status=completed")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(commands["data"]["instance_id"], "local");
    assert_eq!(commands["data"]["sort"], "requested_at_desc");
    assert_eq!(commands["data"]["filters"]["status"], "completed");
    assert_eq!(commands["data"]["items"][0]["command_id"], "cmd_pause_new");
    assert_eq!(commands["data"]["items"][1]["command_id"], "cmd_pause_old");

    let alerts = decode_json::<Value>(
        app,
        Request::builder()
            .uri("/query/alerts?category=risk&sort=created_at_asc")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(alerts["data"]["filters"]["category"], "risk");
    assert_eq!(alerts["data"]["sort"], "created_at_asc");
    assert_eq!(alerts["data"]["items"][0]["code"], "MARGIN_USAGE_WATCH");
    assert_eq!(alerts["data"]["items"][1]["code"], "STOP_LOSS_TRIGGERED");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_plane_capabilities_expose_web_ui_boundary() -> Result<()> {
    let app = build_app(Application::bootstrap());

    let capabilities = decode_json::<Value>(
        app,
        Request::builder()
            .uri("/control-plane/capabilities")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    assert_eq!(capabilities["data"]["instance_id"], "local");
    assert_eq!(capabilities["data"]["deployment"]["mode"], "lan");
    assert_eq!(
        capabilities["data"]["auth"]["mode"],
        "optional_static_token"
    );
    assert_eq!(
        capabilities["data"]["auth"]["http"]["header"],
        "authorization"
    );
    assert_eq!(
        capabilities["data"]["auth"]["http"]["query_param"],
        "access_token"
    );
    assert_eq!(capabilities["data"]["websocket"]["path"], "/ws");
    assert_eq!(
        capabilities["data"]["websocket"]["auth"]["query_param"],
        "access_token"
    );
    assert_eq!(
        capabilities["data"]["websocket"]["subscriptions"][0],
        "runtime_stream"
    );
    assert!(
        capabilities["data"]["endpoint_groups"]
            .as_array()
            .expect("endpoint groups array")
            .iter()
            .any(|group| group["name"] == "commands")
    );

    Ok(())
}

async fn decode_json<T>(app: Router, request: Request<Body>) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let response = app.oneshot(request).await.context("route call failed")?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    serde_json::from_slice(&body).with_context(|| {
        format!(
            "failed to decode json body with status {status}: {}",
            String::from_utf8_lossy(&body)
        )
    })
}

async fn next_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<ServerEnvelope> {
    let message = timeout(Duration::from_secs(3), ws.next())
        .await
        .context("timed out waiting for websocket event")?
        .context("websocket closed unexpectedly")?
        .context("failed to read websocket frame")?;
    let text = message
        .into_text()
        .context("expected text websocket frame")?;
    serde_json::from_str(&text).context("failed to decode websocket event")
}

struct TestServer {
    base_url: String,
    ws_url: String,
    http: reqwest::Client,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn spawn() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind test listener")?;
        let addr = listener.local_addr().context("failed to read local addr")?;
        let application = Application::bootstrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, build_app(application)).await.ok();
        });
        let base_url = format!("http://{addr}");
        let ws_url = format!("ws://{addr}/ws");
        let http = reqwest::Client::new();

        wait_until_ready(&http, &base_url).await?;

        Ok(Self {
            base_url,
            ws_url,
            http,
            task,
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_until_ready(http: &reqwest::Client, base_url: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let url = format!("{base_url}/runtime/snapshot");
    loop {
        if let Ok(response) = http.get(&url).send().await
            && response.status().is_success()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for test service");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn app_with_persisted_runtime(runtime: PersistedRuntime) -> Result<(TempDir, Router)> {
    let temp_dir = tempfile::tempdir()?;
    let db_path = temp_dir.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;
    storage.persist_runtime(&runtime)?;
    let app = build_app(Application::bootstrap_with_sqlite(&db_path)?);
    Ok((temp_dir, app))
}

fn seed_query_runtime() -> PersistedRuntime {
    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.execution.recent_commands = vec![
        CommandRecord {
            command_id: "cmd_pause_old".into(),
            command: CommandType::Pause,
            status: CommandStatus::Completed,
            summary: "Strategy paused.".into(),
            requested_at: "2025-01-01T00:00:10Z".into(),
            accepted_at: Some("2025-01-01T00:00:11Z".into()),
            finished_at: Some("2025-01-01T00:00:12Z".into()),
            links: CommandLinks::default(),
        },
        CommandRecord {
            command_id: "cmd_pause_new".into(),
            command: CommandType::Pause,
            status: CommandStatus::Completed,
            summary: "Strategy paused again.".into(),
            requested_at: "2025-01-01T00:00:20Z".into(),
            accepted_at: Some("2025-01-01T00:00:21Z".into()),
            finished_at: Some("2025-01-01T00:00:22Z".into()),
            links: CommandLinks::default(),
        },
    ];

    PersistedRuntime {
        snapshot,
        risk_events: vec![
            RiskEvent {
                severity: RiskLevel::Watch,
                code: "MARGIN_USAGE_WATCH".into(),
                message: "Margin usage reached 39% of configured threshold.".into(),
                created_at: "2025-01-01T00:00:01Z".into(),
                acknowledged_at: None,
            },
            RiskEvent {
                severity: RiskLevel::Danger,
                code: "STOP_LOSS_TRIGGERED".into(),
                message: "Stop-loss threshold reached.".into(),
                created_at: "2025-01-01T00:00:03Z".into(),
                acknowledged_at: Some("2025-01-01T00:00:05Z".into()),
            },
        ],
        system_events: vec![
            SystemEvent {
                level: "info".into(),
                source: "bootstrap".into(),
                message: "Runtime restored for web query tests.".into(),
                created_at: "2025-01-01T00:00:02Z".into(),
            },
            SystemEvent {
                level: "warn".into(),
                source: "ws".into(),
                message: "WebSocket connection recovering.".into(),
                created_at: "2025-01-01T00:00:04Z".into(),
            },
        ],
        last_sequence: 9,
    }
}
