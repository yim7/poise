use std::time::Duration;

use anyhow::{Context, Result};
use axum::http::StatusCode;
use axum::{Router, body::Body, http::Request};
use futures_util::StreamExt;
use grid_platform_service::{
    Application, ApplicationRegistry, build_app,
    protocol::{
        CommandAccepted, CommandRequest, CommandType, GridConfig, HttpErrorEnvelope,
        HttpSuccessEnvelope, RuntimeSnapshot, ServerEnvelope, ServerEvent,
    },
    storage::PersistedRuntime,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::{
    net::TcpListener,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tower::ServiceExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instances_endpoint_lists_symbols_from_registry() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let response = decode_json::<HttpSuccessEnvelope<Value>>(
        app,
        Request::builder()
            .uri("/instances")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    let symbols = response.data["instances"]
        .as_array()
        .expect("instances array")
        .iter()
        .filter_map(|instance| instance["symbol"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(symbols, vec!["BTCUSDT", "ETHUSDT"]);
    assert_eq!(response.data["default_symbol"], "BTCUSDT");
    assert_eq!(response.data["environment"], "testnet");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_scoped_snapshot_returns_target_symbol_only() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let snapshot = decode_json::<HttpSuccessEnvelope<RuntimeSnapshot>>(
        app,
        Request::builder()
            .uri("/instances/ETHUSDT/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    assert_eq!(snapshot.data.runtime.symbol, "ETHUSDT");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_runtime_snapshot_alias_uses_default_symbol() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let snapshot = decode_json::<HttpSuccessEnvelope<RuntimeSnapshot>>(
        app,
        Request::builder()
            .uri("/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    assert_eq!(snapshot.data.runtime.symbol, "BTCUSDT");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_scoped_pause_only_updates_target_symbol() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let accepted = decode_json::<HttpSuccessEnvelope<CommandAccepted>>(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/instances/ETHUSDT/commands/pause")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"command_id":"cmd_pause_eth"}"#))
            .expect("request"),
    )
    .await?;
    assert_eq!(accepted.data.command, CommandType::Pause);
    assert_eq!(accepted.data.command_id, "cmd_pause_eth");

    let eth_snapshot = decode_json::<HttpSuccessEnvelope<RuntimeSnapshot>>(
        app.clone(),
        Request::builder()
            .uri("/instances/ETHUSDT/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(eth_snapshot.data.runtime.symbol, "ETHUSDT");
    assert_eq!(eth_snapshot.data.runtime.strategy_state, "paused");
    assert!(
        eth_snapshot
            .data
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| ack.command_id == "cmd_pause_eth")
    );

    let btc_snapshot = decode_json::<HttpSuccessEnvelope<RuntimeSnapshot>>(
        app,
        Request::builder()
            .uri("/instances/BTCUSDT/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;
    assert_eq!(btc_snapshot.data.runtime.symbol, "BTCUSDT");
    assert_eq!(btc_snapshot.data.runtime.strategy_state, "running");
    assert!(
        btc_snapshot
            .data
            .execution
            .last_command_ack_event
            .as_ref()
            .is_none()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_instance_symbol_returns_instance_not_found() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/instances/SOLUSDT/commands/pause")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"command_id":"cmd_pause_missing"}"#))
                .expect("request"),
        )
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let bytes = response.into_body().collect().await?.to_bytes();
    let error: HttpErrorEnvelope = serde_json::from_slice(&bytes)?;
    assert_eq!(error.error.code, "instance_not_found");
    assert_eq!(error.error.message, "instance `SOLUSDT` was not found");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_scoped_websocket_streams_target_symbol_snapshot() -> Result<()> {
    let server = MultiInstanceTestServer::spawn(["BTCUSDT", "ETHUSDT"]).await?;
    let (mut ws, _) = connect_async(format!("{}/instances/ETHUSDT/ws", server.ws_base_url))
        .await
        .context("failed to connect instance websocket")?;

    let initial = next_event(&mut ws).await?;
    assert!(initial.sequence.is_some());
    match initial.event {
        ServerEvent::RuntimeSnapshot(snapshot) => {
            assert_eq!(snapshot.runtime.symbol, "ETHUSDT");
        }
        other => panic!("unexpected initial event: {other:?}"),
    }

    server
        .http
        .post(format!(
            "{}/instances/ETHUSDT/commands/pause",
            server.base_url
        ))
        .json(&CommandRequest {
            command_id: "cmd_pause_ws_eth".into(),
        })
        .send()
        .await
        .context("failed to send pause command")?
        .error_for_status()
        .context("pause command returned non-success")?;

    let ack = next_event(&mut ws).await?;
    assert!(ack.sequence.is_some());
    match ack.event {
        ServerEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_pause_ws_eth");
            assert_eq!(ack.command, CommandType::Pause);
        }
        other => panic!("unexpected websocket event: {other:?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_scoped_capabilities_advertise_instance_prefixed_routes() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let capabilities = decode_json::<HttpSuccessEnvelope<Value>>(
        app,
        Request::builder()
            .uri("/instances/ETHUSDT/control-plane/capabilities")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    assert_eq!(capabilities.data["instance_id"], "ETHUSDT");
    assert_eq!(capabilities.data["deployment"]["scope"], "instance_scoped");
    assert_eq!(
        capabilities.data["websocket"]["path"],
        "/instances/ETHUSDT/ws"
    );
    assert!(
        capabilities.data["endpoint_groups"]
            .as_array()
            .expect("endpoint groups")
            .iter()
            .any(|group| {
                group["name"] == "runtime"
                    && group["paths"].as_array().is_some_and(|paths| {
                        paths
                            .iter()
                            .any(|path| path == "/instances/ETHUSDT/runtime/snapshot")
                    })
            })
    );
    assert!(
        capabilities.data["endpoint_groups"]
            .as_array()
            .expect("endpoint groups")
            .iter()
            .any(|group| {
                group["name"] == "commands"
                    && group["paths"].as_array().is_some_and(|paths| {
                        paths
                            .iter()
                            .any(|path| path == "/instances/ETHUSDT/commands/pause")
                    })
            })
    );

    Ok(())
}

fn bootstrap_multi_instance_app<const N: usize>(symbols: [&str; N]) -> Result<Router> {
    let instances = symbols
        .into_iter()
        .map(|symbol| {
            (
                symbol.to_string(),
                Application::bootstrap_with_runtime(seed_runtime(symbol), symbol),
            )
        })
        .collect::<Vec<_>>();
    let registry = ApplicationRegistry::new("testnet", "BTCUSDT", instances)?;
    Ok(build_app(registry))
}

fn seed_runtime(symbol: &str) -> PersistedRuntime {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::empty_bootstrap();
    runtime.snapshot.runtime.symbol = symbol.into();
    runtime.snapshot.runtime.env = "testnet".into();
    runtime.snapshot.runtime.last_price = 100.0;
    runtime.snapshot.runtime.mark_price = 100.0;
    runtime.snapshot.strategy.config = GridConfig {
        lower_price: 90.0,
        upper_price: 110.0,
        grid_levels: 6,
        max_position_notional: 3000.0,
        exchange_rules: None,
    };
    runtime.snapshot.execution.open_orders.clear();
    runtime.snapshot.execution.recent_fills.clear();
    runtime
}

async fn decode_json<T>(app: Router, request: Request<Body>) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let response = app.oneshot(request).await?;
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

struct MultiInstanceTestServer {
    base_url: String,
    ws_base_url: String,
    http: reqwest::Client,
    task: JoinHandle<()>,
}

impl MultiInstanceTestServer {
    async fn spawn<const N: usize>(symbols: [&str; N]) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind test listener")?;
        let addr = listener.local_addr().context("failed to read local addr")?;
        let app = bootstrap_multi_instance_app(symbols)?;
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let base_url = format!("http://{addr}");
        let ws_base_url = format!("ws://{addr}");
        let http = reqwest::Client::new();

        wait_until_ready(&http, &base_url).await?;

        Ok(Self {
            base_url,
            ws_base_url,
            http,
            task,
        })
    }
}

impl Drop for MultiInstanceTestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_until_ready(http: &reqwest::Client, base_url: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let url = format!("{base_url}/instances");
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

async fn next_event(
    ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) -> Result<ServerEnvelope> {
    let message = timeout(Duration::from_secs(2), ws.next())
        .await
        .context("timed out waiting for websocket event")?
        .context("websocket closed unexpectedly")?
        .context("failed to read websocket frame")?;
    let text = message
        .into_text()
        .context("expected text websocket frame")?;
    serde_json::from_str(&text).context("failed to decode websocket event")
}
