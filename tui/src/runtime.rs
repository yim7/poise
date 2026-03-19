use std::{env, io, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{
    select,
    sync::mpsc,
    time::{interval, sleep},
};

use crate::{
    effects::Effect,
    events::{AppEvent, EffectResultEvent, InputEvent, SystemEvent},
    input::map_key_event,
    render::draw,
    state::AppState,
    theme::Theme,
    transport::TransportClient,
};

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub base_url: String,
    pub ws_url: String,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self {
            base_url: env::var("GRID_PLATFORM_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8000".into()),
            ws_url: env::var("GRID_PLATFORM_WS_URL")
                .unwrap_or_else(|_| "ws://127.0.0.1:8000/ws".into()),
        }
    }
}

pub async fn run_app(config: AppConfig) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;
    terminal.clear().context("failed to clear terminal")?;

    let result = run_loop(&mut terminal, config).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: AppConfig,
) -> Result<()> {
    let theme = Theme::default();
    let mut state = AppState::sample();
    state.connection.ws_connected = false;
    let (app_tx, mut app_rx) = mpsc::channel::<AppEvent>(512);
    let (effect_tx, effect_rx) = mpsc::channel::<Effect>(256);
    let transport = TransportClient::new(config.base_url, config.ws_url);

    tokio::spawn(input_task(app_tx.clone()));
    tokio::spawn(clock_task(app_tx.clone()));
    tokio::spawn(effect_task(transport, app_tx.clone(), effect_rx));
    effect_tx.send(Effect::FetchSnapshot).await.ok();

    terminal.draw(|frame| draw(frame, &state, &theme))?;
    state.dirty.clear();

    while let Some(event) = app_rx.recv().await {
        let is_render_tick = matches!(&event, AppEvent::System(SystemEvent::RenderTick));
        let effects = crate::store::reduce(&mut state, event);
        for effect in effects {
            effect_tx.send(effect).await.ok();
        }
        if state.ui.should_quit {
            break;
        }
        if state.take_immediate_render() || (is_render_tick && state.dirty.any()) {
            terminal.draw(|frame| draw(frame, &state, &theme))?;
            state.dirty.clear();
        }
    }

    Ok(())
}

async fn input_task(app_tx: mpsc::Sender<AppEvent>) {
    let mut stream = EventStream::new();
    while let Some(Ok(event)) = stream.next().await {
        match event {
            Event::Key(key) => {
                if let Some(action) = map_key_event(key) {
                    if app_tx
                        .send(AppEvent::Input(InputEvent::Key(action)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Event::Resize(width, height) => {
                if app_tx
                    .send(AppEvent::Input(InputEvent::Resize(width, height)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            _ => {}
        }
    }
}

async fn clock_task(app_tx: mpsc::Sender<AppEvent>) {
    let mut render = interval(Duration::from_millis(83));
    let mut health = interval(Duration::from_secs(1));
    loop {
        select! {
            _ = render.tick() => {
                if app_tx.send(AppEvent::System(SystemEvent::RenderTick)).await.is_err() {
                    break;
                }
            }
            _ = health.tick() => {
                if app_tx.send(AppEvent::System(SystemEvent::HealthTick)).await.is_err() {
                    break;
                }
            }
        }
    }
}

async fn effect_task(
    transport: TransportClient,
    app_tx: mpsc::Sender<AppEvent>,
    mut effect_rx: mpsc::Receiver<Effect>,
) {
    while let Some(effect) = effect_rx.recv().await {
        match effect {
            Effect::FetchSnapshot => match transport.fetch_snapshot().await {
                Ok(snapshot) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::SnapshotLoaded(
                            snapshot,
                        )))
                        .await;
                }
                Err(error) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::SnapshotFailed(
                            error.to_string(),
                        )))
                        .await;
                }
            },
            Effect::ConnectWs => {
                transport.spawn_ws_listener(app_tx.clone());
            }
            Effect::ReconnectWs { attempt } => {
                let backoff_secs = 2u64.saturating_pow(attempt.saturating_sub(1)).min(8);
                sleep(Duration::from_secs(backoff_secs)).await;
                transport.spawn_ws_listener(app_tx.clone());
            }
            Effect::SendCommand {
                command,
                command_id,
            } => match transport.send_command(command, command_id.clone()).await {
                Ok(accepted) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::CommandAccepted(
                            accepted,
                        )))
                        .await;
                }
                Err(error) => {
                    let _ = app_tx
                        .send(AppEvent::EffectResult(EffectResultEvent::CommandFailed {
                            command_id,
                            error: error.to_string(),
                        }))
                        .await;
                }
            },
            Effect::LogClientSideEvent(_) => {}
        }
    }
}
