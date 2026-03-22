use std::{
    collections::VecDeque,
    net::TcpListener,
    path::Path,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use grid_platform_tui::{
    effects::Effect,
    events::{AppEvent, EffectResultEvent, InputEvent, KeyAction},
    locale::Locale,
    protocol::{CommandRequest, CommandType, InstanceSummary, InstancesDirectory, RuntimeSnapshot},
    render::draw,
    state::{AppState, CommandTimelineStage, Page, SnapshotBootstrapState},
    store::reduce,
    theme::Theme,
    transport::TransportClient,
};
use ratatui::{Terminal, backend::TestBackend};
use tempfile::{TempDir, tempdir};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener as TokioTcpListener,
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout},
};

struct ServiceProcess {
    child: Child,
    base_url: String,
    ws_url: String,
    http: reqwest::Client,
    _db_temp_dir: Option<TempDir>,
    _run_dir: TempDir,
}

impl ServiceProcess {
    async fn start() -> Result<Self> {
        let temp_dir = tempdir()?;
        let db_path = temp_dir.path().join("service.db");
        Self::start_inner(Some(db_path.as_path()), Some(temp_dir)).await
    }

    async fn start_with_db(db_path: &Path) -> Result<Self> {
        Self::start_inner(Some(db_path), None).await
    }

    async fn start_inner(db_path: Option<&Path>, temp_dir: Option<TempDir>) -> Result<Self> {
        let _startup_guard = service_start_lock().lock().await;
        let port = reserve_port()?;
        let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let run_dir = tempdir()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let http = reqwest::Client::new();
        let service_bin = workspace_dir.join("target/debug/grid-platform-service");

        let mut command = Command::new(&service_bin);
        command
            .current_dir(run_dir.path())
            .env_clear()
            .env("GRID_PLATFORM_SERVICE_ADDR", format!("127.0.0.1:{port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(db_path) = db_path {
            command.env("GRID_PLATFORM_SERVICE_DB_PATH", db_path);
        }

        let mut child = command
            .spawn()
            .context("failed to spawn local paper service")?;

        wait_until_ready(&http, &base_url, &mut child).await?;

        Ok(Self {
            child,
            base_url,
            ws_url,
            http,
            _db_temp_dir: temp_dir,
            _run_dir: run_dir,
        })
    }

    async fn emit_price_tick(&self) -> Result<()> {
        self.http
            .post(format!("{}/__test__/emit-price-tick", self.base_url))
            .send()
            .await
            .context("failed to call test price tick endpoint")?
            .error_for_status()
            .context("test price tick endpoint returned non-success status")?;
        Ok(())
    }

    async fn send_command(&self, command: CommandType, command_id: &str) -> Result<()> {
        let path = match command {
            CommandType::Pause => "pause",
            CommandType::Resume => "resume",
            CommandType::CancelAll => "cancel-all",
            CommandType::FlattenNow => "flatten-now",
            CommandType::ShutdownAfterFlatten => "shutdown-after-flatten",
        };

        self.http
            .post(format!("{}/commands/{path}", self.base_url))
            .json(&CommandRequest {
                command_id: command_id.into(),
            })
            .send()
            .await
            .context("failed to call command endpoint")?
            .error_for_status()
            .context("command endpoint returned non-success status")?;
        Ok(())
    }
}

fn service_start_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct AppHarness {
    state: AppState,
    transport: TransportClient,
    app_tx: mpsc::Sender<AppEvent>,
    app_rx: mpsc::Receiver<AppEvent>,
    ws_task: Option<JoinHandle<()>>,
}

impl AppHarness {
    async fn new_waiting(base_url: String, ws_url: String) -> Result<Self> {
        let transport = TransportClient::new(base_url, ws_url);
        let (app_tx, app_rx) = mpsc::channel(64);
        Ok(Self {
            state: AppState::waiting_first_snapshot(),
            transport,
            app_tx,
            app_rx,
            ws_task: None,
        })
    }

    async fn connect(base_url: String, ws_url: String) -> Result<Self> {
        let mut harness = Self::new_waiting(base_url, ws_url).await?;
        let effects = harness.bootstrap_once().await?;
        harness.apply_effects(effects).await?;
        harness
            .drive_until(
                |state| {
                    matches!(state.snapshot_state, SnapshotBootstrapState::Ready)
                        && state.connection.ws_connected
                        && state.runtime.symbol == "XAUUSDT"
                },
                Duration::from_secs(3),
            )
            .await?;
        Ok(harness)
    }

    async fn bootstrap_once(&mut self) -> Result<Vec<Effect>> {
        let directory = self.transport.fetch_instances().await?;
        let mut pending = VecDeque::from(reduce(
            &mut self.state,
            AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(directory)),
        ));
        let mut next_effects = Vec::new();

        while let Some(effect) = pending.pop_front() {
            match effect {
                Effect::UseInstance {
                    symbol: _,
                    generation: _,
                } => {
                    if let Some(task) = self.ws_task.take() {
                        task.abort();
                    }
                }
                Effect::FetchSnapshot { symbol, generation } => {
                    let snapshot = self.transport.fetch_instance_snapshot(&symbol).await?;
                    next_effects.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                            symbol,
                            generation,
                            snapshot,
                        }),
                    ));
                }
                other => next_effects.push(other),
            }
        }

        Ok(next_effects)
    }

    async fn submit_key(&mut self, action: KeyAction) -> Result<()> {
        let effects = reduce(&mut self.state, AppEvent::Input(InputEvent::Key(action)));
        self.apply_effects(effects).await
    }

    fn force_ws_disconnect(&mut self, reason: &str) -> Vec<Effect> {
        if let Some(task) = self.ws_task.take() {
            task.abort();
        }
        let symbol = self
            .state
            .instances
            .current_symbol
            .clone()
            .or_else(|| {
                (!self.state.runtime.symbol.is_empty()).then(|| self.state.runtime.symbol.clone())
            })
            .unwrap_or_default();
        let generation = self.state.instances.generation;
        reduce(
            &mut self.state,
            AppEvent::EffectResult(EffectResultEvent::WsDisconnected {
                symbol,
                generation,
                reason: reason.into(),
            }),
        )
    }

    async fn drive_until<F>(&mut self, predicate: F, within: Duration) -> Result<()>
    where
        F: Fn(&AppState) -> bool,
    {
        if predicate(&self.state) {
            return Ok(());
        }

        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.pump_once(Duration::from_millis(100)).await? && predicate(&self.state) {
                return Ok(());
            }
        }

        Err(anyhow!("timed out waiting for expected E2E state"))
    }

    async fn pump_once(&mut self, wait_for: Duration) -> Result<bool> {
        match timeout(wait_for, self.app_rx.recv()).await {
            Ok(Some(event)) => {
                let effects = reduce(&mut self.state, event);
                self.apply_effects(effects).await?;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(_) => Ok(false),
        }
    }

    async fn apply_effects(&mut self, effects: Vec<Effect>) -> Result<()> {
        let mut pending = VecDeque::from(effects);
        while let Some(effect) = pending.pop_front() {
            match effect {
                Effect::FetchInstances => {
                    let directory = self.transport.fetch_instances().await?;
                    pending.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(directory)),
                    ));
                }
                Effect::FetchInstancesAfterDelay { retry_in_ms } => {
                    sleep(Duration::from_millis(retry_in_ms)).await;
                    let directory = self.transport.fetch_instances().await?;
                    pending.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::InstancesLoaded(directory)),
                    ));
                }
                Effect::UseInstance {
                    symbol: _,
                    generation: _,
                } => {
                    if let Some(task) = self.ws_task.take() {
                        task.abort();
                    }
                }
                Effect::FetchSnapshot { symbol, generation } => {
                    let snapshot = self.transport.fetch_instance_snapshot(&symbol).await?;
                    pending.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                            symbol,
                            generation,
                            snapshot,
                        }),
                    ));
                }
                Effect::FetchSnapshotAfterDelay {
                    symbol,
                    generation,
                    retry_in_ms,
                } => {
                    sleep(Duration::from_millis(retry_in_ms)).await;
                    let snapshot = self.transport.fetch_instance_snapshot(&symbol).await?;
                    pending.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded {
                            symbol,
                            generation,
                            snapshot,
                        }),
                    ));
                }
                Effect::FetchRiskEvents { symbol, generation } => {
                    let alerts = self.transport.fetch_instance_risk_events(&symbol).await?;
                    pending.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::RiskEventsLoaded {
                            symbol,
                            generation,
                            alerts,
                        }),
                    ));
                }
                Effect::ConnectWs { symbol, generation } => {
                    if let Some(task) = self.ws_task.take() {
                        task.abort();
                    }
                    self.ws_task = Some(self.transport.spawn_instance_ws_listener(
                        symbol,
                        generation,
                        self.app_tx.clone(),
                    ));
                }
                Effect::ReconnectWs {
                    symbol,
                    generation,
                    attempt,
                } => {
                    let backoff_secs = 2u64.saturating_pow(attempt.saturating_sub(1)).min(8);
                    sleep(Duration::from_secs(backoff_secs)).await;
                    if let Some(task) = self.ws_task.take() {
                        task.abort();
                    }
                    self.ws_task = Some(self.transport.spawn_instance_ws_listener(
                        symbol,
                        generation,
                        self.app_tx.clone(),
                    ));
                }
                Effect::SendCommand {
                    symbol,
                    generation,
                    command,
                    command_id,
                } => {
                    let accepted = self
                        .transport
                        .send_instance_command(&symbol, command, command_id)
                        .await?;
                    pending.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::CommandAccepted {
                            symbol,
                            generation,
                            accepted,
                        }),
                    ));
                }
                Effect::LogClientSideEvent(_) => {}
            }
        }
        Ok(())
    }
}

impl Drop for AppHarness {
    fn drop(&mut self) {
        if let Some(task) = self.ws_task.take() {
            task.abort();
        }
    }
}

struct RecordingHttpServer {
    base_url: String,
    recorded_paths: Arc<tokio::sync::Mutex<Vec<String>>>,
    handle: JoinHandle<()>,
}

impl RecordingHttpServer {
    async fn start() -> Result<Self> {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind recording server")?;
        let port = listener
            .local_addr()
            .context("failed to read recording server addr")?
            .port();
        let base_url = format!("http://127.0.0.1:{port}");
        let recorded_paths = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let paths = Arc::clone(&recorded_paths);

        let handle = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let paths = Arc::clone(&paths);
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 4096];
                    let mut request = Vec::new();
                    loop {
                        match socket.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(count) => {
                                request.extend_from_slice(&buffer[..count]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }

                    let request_text = String::from_utf8_lossy(&request);
                    let path = request_text
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    paths.lock().await.push(path.clone());

                    let (status, body) = response_body_for_path(&path);
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Ok(Self {
            base_url,
            recorded_paths,
            handle,
        })
    }

    async fn recorded_paths(&self) -> Vec<String> {
        self.recorded_paths.lock().await.clone()
    }
}

impl Drop for RecordingHttpServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn response_body_for_path(path: &str) -> (&'static str, String) {
    match path {
        "/instances" => {
            let directory = InstancesDirectory {
                environment: "testnet".into(),
                default_symbol: "BTCUSDT".into(),
                instances: vec![
                    InstanceSummary {
                        symbol: "BTCUSDT".into(),
                        environment: "testnet".into(),
                        is_default: true,
                    },
                    InstanceSummary {
                        symbol: "ETHUSDT".into(),
                        environment: "testnet".into(),
                        is_default: false,
                    },
                ],
            };
            (
                "200 OK",
                serde_json::json!({
                    "version": "1",
                    "status": "ok",
                    "data": directory,
                })
                .to_string(),
            )
        }
        "/runtime/snapshot" => (
            "200 OK",
            serde_json::json!({
                "version": "1",
                "status": "ok",
                "data": RuntimeSnapshot::sample(),
            })
            .to_string(),
        ),
        path if path.starts_with("/instances/") && path.ends_with("/runtime/snapshot") => (
            "200 OK",
            serde_json::json!({
                "version": "1",
                "status": "ok",
                "data": RuntimeSnapshot::sample(),
            })
            .to_string(),
        ),
        "/risk/events" => (
            "200 OK",
            serde_json::json!({
                "version": "1",
                "status": "ok",
                "data": []
            })
            .to_string(),
        ),
        _ => (
            "404 Not Found",
            serde_json::json!({
                "version": "1",
                "status": "error",
                "error": {
                    "code": "not_found",
                    "message": format!("unhandled path: {path}"),
                    "details": null
                }
            })
            .to_string(),
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_bootstrap_should_load_instances_before_default_snapshot() -> Result<()> {
    let server = RecordingHttpServer::start().await?;
    let mut app =
        AppHarness::new_waiting(server.base_url.clone(), "ws://127.0.0.1:0/ws".into()).await?;

    let _ = app.bootstrap_once().await?;

    let paths = server.recorded_paths().await;
    assert_eq!(
        paths,
        vec![
            "/instances".to_string(),
            "/instances/BTCUSDT/runtime/snapshot".to_string()
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_bootstrap_market_tick_and_pause_ack_flow() -> Result<()> {
    let service = ServiceProcess::start().await?;
    let mut app = AppHarness::connect(service.base_url.clone(), service.ws_url.clone()).await?;

    assert!(matches!(
        app.state.snapshot_state,
        SnapshotBootstrapState::Ready
    ));
    assert_eq!(app.state.runtime.symbol, "XAUUSDT");
    assert!(app.state.connection.ws_connected);

    let initial_price = app.state.runtime.last_price;
    service.emit_price_tick().await?;
    app.drive_until(
        |state| state.runtime.last_price > initial_price,
        Duration::from_secs(2),
    )
    .await?;

    app.submit_key(KeyAction::Pause).await?;
    app.drive_until(
        |state| {
            state.runtime.strategy_state == "paused"
                && state
                    .execution
                    .last_command_ack
                    .as_ref()
                    .is_some_and(|ack| ack.command == CommandType::Pause)
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command == CommandType::Pause && entry.stage == CommandTimelineStage::Ack
                })
        },
        Duration::from_secs(3),
    )
    .await?;

    app.submit_key(KeyAction::Resume).await?;
    app.drive_until(
        |state| {
            state.runtime.strategy_state == "running"
                && state
                    .execution
                    .last_command_ack
                    .as_ref()
                    .is_some_and(|ack| ack.command == CommandType::Resume)
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command == CommandType::Resume && entry.stage == CommandTimelineStage::Ack
                })
        },
        Duration::from_secs(3),
    )
    .await?;

    app.submit_key(KeyAction::FlattenNow).await?;
    app.submit_key(KeyAction::Confirm).await?;
    app.drive_until(
        |state| {
            state.runtime.position_qty == 0.0
                && state
                    .execution
                    .last_command_ack
                    .as_ref()
                    .is_some_and(|ack| ack.command == CommandType::FlattenNow)
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command == CommandType::FlattenNow
                        && entry.stage == CommandTimelineStage::Ack
                })
        },
        Duration::from_secs(3),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_first_snapshot_bootstrap_enables_runtime_ops_only_after_ready() -> Result<()> {
    let service = ServiceProcess::start().await?;
    let mut app = AppHarness::new_waiting(service.base_url.clone(), service.ws_url.clone()).await?;

    assert!(matches!(
        app.state.snapshot_state,
        SnapshotBootstrapState::WaitingFirstSnapshot
    ));

    app.submit_key(KeyAction::Pause).await?;
    assert_eq!(
        app.state
            .ui
            .toast
            .as_ref()
            .map(|toast| toast.message.as_str()),
        Some("Initial snapshot pending. Runtime actions are disabled.")
    );
    assert!(matches!(
        app.state.snapshot_state,
        SnapshotBootstrapState::WaitingFirstSnapshot
    ));

    let effects = app.bootstrap_once().await?;
    app.apply_effects(effects).await?;
    app.drive_until(
        |state| matches!(state.snapshot_state, SnapshotBootstrapState::Ready),
        Duration::from_secs(3),
    )
    .await?;

    app.submit_key(KeyAction::Pause).await?;
    app.drive_until(
        |state| {
            state
                .execution
                .last_command_ack
                .as_ref()
                .is_some_and(|ack| ack.command == CommandType::Pause)
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command == CommandType::Pause && entry.stage == CommandTimelineStage::Ack
                })
        },
        Duration::from_secs(3),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_first_snapshot_success_shows_real_empty_state() -> Result<()> {
    let service = ServiceProcess::start().await?;
    let mut app = AppHarness::new_waiting(service.base_url.clone(), service.ws_url.clone()).await?;

    let effects = app.bootstrap_once().await?;

    assert!(matches!(
        app.state.snapshot_state,
        SnapshotBootstrapState::Ready
    ));
    assert_eq!(
        effects,
        vec![
            Effect::FetchRiskEvents {
                symbol: "XAUUSDT".into(),
                generation: 1,
            },
            Effect::ConnectWs {
                symbol: "XAUUSDT".into(),
                generation: 1,
            },
        ]
    );
    assert!(app.state.execution.open_orders.is_empty());
    assert!(app.state.execution.recent_fills.is_empty());
    assert!(app.state.execution.command_timeline.is_empty());
    assert!(app.state.risk.alerts.is_empty());

    app.state.ui.page = Page::Events;
    let rendered = render_state_to_string(&app.state, 100, 24);
    assert!(rendered.contains("No fills"));
    assert!(rendered.contains("No alerts"));
    assert!(rendered.contains("No recent commands"));
    assert!(rendered.contains("No system events"));
    assert!(!rendered.contains("WAITING SNAPSHOT"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_toggle_locale_while_running_keeps_pause_ack_flow_working() -> Result<()> {
    let service = ServiceProcess::start().await?;
    let mut app = AppHarness::connect(service.base_url.clone(), service.ws_url.clone()).await?;

    assert_eq!(app.state.ui.locale, Locale::EnUs);

    app.submit_key(KeyAction::ToggleLocale).await?;
    assert_eq!(app.state.ui.locale, Locale::ZhCn);

    app.submit_key(KeyAction::Pause).await?;
    app.drive_until(
        |state| {
            state.runtime.strategy_state == "paused"
                && state
                    .execution
                    .last_command_ack
                    .as_ref()
                    .is_some_and(|ack| ack.command == CommandType::Pause)
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command == CommandType::Pause && entry.stage == CommandTimelineStage::Ack
                })
        },
        Duration::from_secs(3),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_cancel_all_clears_open_orders_and_records_ack() -> Result<()> {
    let service = ServiceProcess::start().await?;
    let mut app = AppHarness::connect(service.base_url.clone(), service.ws_url.clone()).await?;

    let initial_open_orders = app.state.execution.open_orders.len();

    app.submit_key(KeyAction::CancelAll).await?;
    app.submit_key(KeyAction::Confirm).await?;
    app.drive_until(
        |state| {
            state
                .execution
                .last_command_ack
                .as_ref()
                .is_some_and(|ack| ack.command == CommandType::CancelAll)
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command == CommandType::CancelAll
                        && entry.stage == CommandTimelineStage::Ack
                })
                && (initial_open_orders == 0 || state.execution.open_orders.is_empty())
        },
        Duration::from_secs(3),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_reconnect_resyncs_runtime_snapshot_and_command_result() -> Result<()> {
    let service = ServiceProcess::start().await?;
    let mut app = AppHarness::connect(service.base_url.clone(), service.ws_url.clone()).await?;
    let initial_price = app.state.runtime.last_price;

    let reconnect_effects = app.force_ws_disconnect("forced reconnect for e2e");
    assert!(!app.state.connection.ws_connected);

    service.emit_price_tick().await?;
    app.apply_effects(reconnect_effects).await?;
    app.drive_until(
        |state| {
            state.connection.ws_connected
                && state.snapshot_state == SnapshotBootstrapState::Ready
                && state.runtime.last_price > initial_price
        },
        Duration::from_secs(5),
    )
    .await?;

    app.submit_key(KeyAction::Pause).await?;
    app.drive_until(
        |state| {
            state
                .execution
                .last_command_ack
                .as_ref()
                .is_some_and(|ack| ack.command == CommandType::Pause)
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command == CommandType::Pause && entry.stage == CommandTimelineStage::Ack
                })
        },
        Duration::from_secs(3),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_restart_recovers_cold_start_and_reconnect_flow() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");

    {
        let service = ServiceProcess::start_with_db(&db_path).await?;
        let mut app = AppHarness::connect(service.base_url.clone(), service.ws_url.clone()).await?;

        app.submit_key(KeyAction::Pause).await?;
        app.drive_until(
            |state| {
                state.runtime.strategy_state == "paused"
                    && state
                        .execution
                        .last_command_ack
                        .as_ref()
                        .is_some_and(|ack| ack.command == CommandType::Pause)
                    && state.execution.command_timeline.iter().any(|entry| {
                        entry.command == CommandType::Pause
                            && entry.stage == CommandTimelineStage::Ack
                    })
            },
            Duration::from_secs(3),
        )
        .await?;
    }

    let service = ServiceProcess::start_with_db(&db_path).await?;
    let mut app = AppHarness::connect(service.base_url.clone(), service.ws_url.clone()).await?;

    assert_eq!(app.state.runtime.strategy_state, "paused");
    assert!(
        app.state
            .execution
            .last_command_ack
            .as_ref()
            .is_some_and(|ack| ack.command == CommandType::Pause)
    );
    assert!(app.state.execution.command_timeline.iter().any(|entry| {
        entry.command == CommandType::Pause && entry.stage == CommandTimelineStage::Ack
    }));

    let reconnect_effects = app.force_ws_disconnect("forced reconnect after restart");
    service
        .send_command(CommandType::Resume, "cmd_resume_after_restart")
        .await?;
    app.apply_effects(reconnect_effects).await?;
    app.drive_until(
        |state| {
            state.runtime.strategy_state == "running"
                && state
                    .execution
                    .last_command_ack
                    .as_ref()
                    .is_some_and(|ack| ack.command_id == "cmd_resume_after_restart")
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command_id == "cmd_resume_after_restart"
                        && entry.command == CommandType::Resume
                        && entry.stage == CommandTimelineStage::Ack
                })
        },
        Duration::from_secs(3),
    )
    .await?;

    Ok(())
}

fn reserve_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to reserve local port")?;
    let port = listener
        .local_addr()
        .context("failed to read reserved local port")?
        .port();
    drop(listener);
    Ok(port)
}

async fn wait_until_ready(http: &reqwest::Client, base_url: &str, child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let url = format!("{base_url}/runtime/snapshot");

    loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to poll local paper service process")?
        {
            return Err(anyhow!(
                "local paper service exited early with status {status}"
            ));
        }

        if let Ok(response) = http.get(&url).send().await
            && response.status().is_success()
        {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for local paper service to become ready"
            ));
        }

        sleep(Duration::from_millis(100)).await;
    }
}

fn render_state_to_string(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::default();
    terminal.draw(|frame| draw(frame, state, &theme)).unwrap();
    buffer_to_string(terminal.backend().buffer(), width, height)
}

fn buffer_to_string(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            let cell = buffer.cell((x, y)).unwrap();
            output.push_str(cell.symbol());
        }
        output.push('\n');
    }
    output
}
