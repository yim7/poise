use std::time::Duration;

use anyhow::{Context, Result};
use axum::{Router, body::Body, http::Request};
use futures_util::StreamExt;
use grid_platform_service::{
    Application, build_app,
    protocol::{
        CommandAccepted, CommandAck, CommandRequest, CommandType, HttpSuccessEnvelope,
        RuntimeSnapshot, ServerEnvelope, ServerEvent,
    },
};
use http_body_util::BodyExt;
use tokio::{
    net::TcpListener,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::connect_async;
use tower::ServiceExt;

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

    assert_eq!(snapshot.sequence, event.sequence);
    assert!(
        event
            .sequence
            .zip(snapshot.sequence)
            .is_some_and(|(event_sequence, snapshot_sequence)| event_sequence <= snapshot_sequence)
    );

    Ok(())
}

async fn decode_json<T>(app: Router, request: Request<Body>) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let response = app.oneshot(request).await.context("route call failed")?;
    let body = response.into_body().collect().await?.to_bytes();
    serde_json::from_slice(&body).context("failed to decode json body")
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
