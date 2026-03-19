use std::{
    collections::VecDeque,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use grid_platform_tui::{
    effects::Effect,
    events::{AppEvent, EffectResultEvent, InputEvent, KeyAction},
    protocol::{CommandRequest, CommandType},
    state::{AppState, CommandTimelineStage},
    store::reduce,
    transport::TransportClient,
};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout},
};

struct ServiceProcess {
    child: Child,
    base_url: String,
    ws_url: String,
    http: reqwest::Client,
}

impl ServiceProcess {
    async fn start() -> Result<Self> {
        let port = reserve_port()?;
        let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let base_url = format!("http://127.0.0.1:{port}");
        let ws_url = format!("ws://127.0.0.1:{port}/ws");
        let http = reqwest::Client::new();

        let mut child = Command::new("cargo")
            .arg("run")
            .arg("-p")
            .arg("grid-platform-service")
            .current_dir(&workspace_dir)
            .env("GRID_PLATFORM_SERVICE_ADDR", format!("127.0.0.1:{port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn local paper service")?;

        wait_until_ready(&http, &base_url, &mut child).await?;

        Ok(Self {
            child,
            base_url,
            ws_url,
            http,
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
    async fn connect(base_url: String, ws_url: String) -> Result<Self> {
        let transport = TransportClient::new(base_url, ws_url);
        let (app_tx, app_rx) = mpsc::channel(64);
        let mut harness = Self {
            state: AppState::sample(),
            transport,
            app_tx,
            app_rx,
            ws_task: None,
        };
        harness.state.connection.ws_connected = false;

        let snapshot = harness.transport.fetch_snapshot().await?;
        let effects = reduce(
            &mut harness.state,
            AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded(snapshot)),
        );
        harness.apply_effects(effects).await?;
        harness
            .drive_until(
                |state| state.connection.ws_connected && state.runtime.symbol == "XAUUSDT",
                Duration::from_secs(3),
            )
            .await?;
        Ok(harness)
    }

    async fn submit_key(&mut self, action: KeyAction) -> Result<()> {
        let effects = reduce(&mut self.state, AppEvent::Input(InputEvent::Key(action)));
        self.apply_effects(effects).await
    }

    fn force_ws_disconnect(&mut self, reason: &str) -> Vec<Effect> {
        if let Some(task) = self.ws_task.take() {
            task.abort();
        }
        reduce(
            &mut self.state,
            AppEvent::EffectResult(EffectResultEvent::WsDisconnected(reason.into())),
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
                Effect::FetchSnapshot => {
                    let snapshot = self.transport.fetch_snapshot().await?;
                    pending.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded(snapshot)),
                    ));
                }
                Effect::ConnectWs => {
                    if let Some(task) = self.ws_task.take() {
                        task.abort();
                    }
                    self.ws_task = Some(self.transport.spawn_ws_listener(self.app_tx.clone()));
                }
                Effect::ReconnectWs { attempt } => {
                    let backoff_secs = 2u64.saturating_pow(attempt.saturating_sub(1)).min(8);
                    sleep(Duration::from_secs(backoff_secs)).await;
                    if let Some(task) = self.ws_task.take() {
                        task.abort();
                    }
                    self.ws_task = Some(self.transport.spawn_ws_listener(self.app_tx.clone()));
                }
                Effect::SendCommand {
                    command,
                    command_id,
                } => {
                    let accepted = self.transport.send_command(command, command_id).await?;
                    pending.extend(reduce(
                        &mut self.state,
                        AppEvent::EffectResult(EffectResultEvent::CommandAccepted(accepted)),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_paper_bootstrap_market_tick_and_pause_ack_flow() -> Result<()> {
    let service = ServiceProcess::start().await?;
    let mut app = AppHarness::connect(service.base_url.clone(), service.ws_url.clone()).await?;

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
async fn local_paper_reconnect_resyncs_runtime_snapshot_and_command_result() -> Result<()> {
    let service = ServiceProcess::start().await?;
    let mut app = AppHarness::connect(service.base_url.clone(), service.ws_url.clone()).await?;
    let initial_price = app.state.runtime.last_price;

    let reconnect_effects = app.force_ws_disconnect("forced reconnect for e2e");
    assert!(!app.state.connection.ws_connected);

    service
        .send_command(CommandType::FlattenNow, "cmd_flatten_reconnect")
        .await?;
    service.emit_price_tick().await?;
    app.apply_effects(reconnect_effects).await?;
    app.drive_until(
        |state| {
            state.connection.ws_connected
                && state.runtime.last_price > initial_price
                && state.runtime.position_qty == 0.0
                && state
                    .execution
                    .last_command_ack
                    .as_ref()
                    .is_some_and(|ack| {
                        ack.command_id == "cmd_flatten_reconnect"
                            && ack.command == CommandType::FlattenNow
                    })
                && state.execution.command_timeline.iter().any(|entry| {
                    entry.command_id == "cmd_flatten_reconnect"
                        && entry.command == CommandType::FlattenNow
                        && entry.stage == CommandTimelineStage::Ack
                })
        },
        Duration::from_secs(5),
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
