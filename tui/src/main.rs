mod api_client;
mod app;
mod input;
mod protocol;
mod signal;
mod theme;
mod views;

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api_client::{ApiClient, connect_ws};
use crate::app::{App, View};
use crate::input::{Action, CommandKind, handle_key_event};
use crate::protocol::{AccountSummaryView, StreamEvent, TrackCommandAccepted};
use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8000";
const INITIAL_LOAD_RETRY_DELAY: Duration = Duration::from_millis(500);
const WS_RECONNECT_DELAY: Duration = Duration::from_millis(500);

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeConfig {
    base_url: String,
    ws_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TracingDestination {
    Disabled,
    Stderr,
    File(PathBuf),
}

struct TerminalGuard {
    terminal: AppTerminal,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error).context("failed to enter alternate screen");
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error).context("failed to create terminal");
            }
        };
        if let Err(error) = terminal.hide_cursor() {
            let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error).context("failed to hide cursor");
        }

        Ok(Self { terminal })
    }

    fn terminal_mut(&mut self) -> &mut AppTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;
    let config = RuntimeConfig::from_env()?;
    let mut terminal = TerminalGuard::new()?;
    let client = ApiClient::new(config.base_url.clone());
    let (mut app, mut ws_receiver) = bootstrap_runtime_state(&client, &config.ws_url).await;

    run_loop(
        terminal.terminal_mut(),
        &client,
        &config.ws_url,
        &mut app,
        &mut ws_receiver,
    )
    .await
}

fn init_tracing() -> Result<()> {
    match tracing_destination_from_env() {
        TracingDestination::Disabled => Ok(()),
        TracingDestination::Stderr => {
            let _ = tracing_subscriber::fmt().with_writer(io::stderr).try_init();
            Ok(())
        }
        TracingDestination::File(path) => init_file_tracing(&path),
    }
}

fn init_file_tracing(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open tracing log file `{}`", path.display()))?;
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || {
            file.try_clone()
                .expect("failed to clone tracing log file handle")
        })
        .try_init();
    Ok(())
}

fn tracing_destination_from_env() -> TracingDestination {
    parse_tracing_destination(env_value("POISE_TUI_LOG").as_deref())
}

fn parse_tracing_destination(value: Option<&str>) -> TracingDestination {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => TracingDestination::Disabled,
        Some(value) if value.eq_ignore_ascii_case("stderr") => TracingDestination::Stderr,
        Some(path) => TracingDestination::File(PathBuf::from(path)),
    }
}

impl RuntimeConfig {
    fn from_env() -> Result<Self> {
        let base_url = env_value("POISE_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let ws_url = match env_value("POISE_TUI_WS_URL").or_else(|| env_value("POISE_WS_URL")) {
            Some(url) => url,
            None => derive_ws_url(&base_url)
                .with_context(|| format!("failed to derive websocket url from `{base_url}`"))?,
        };

        Ok(Self { base_url, ws_url })
    }
}

fn env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn derive_ws_url(base_url: &str) -> Result<String> {
    let mut url = url::Url::parse(base_url).context("failed to parse base url")?;
    match url.scheme() {
        "http" => url.set_scheme("ws").ok(),
        "https" => url.set_scheme("wss").ok(),
        other => anyhow::bail!("unsupported base url scheme `{other}`"),
    };
    let base_path = url.path().trim_end_matches('/');
    let ws_path = if base_path.is_empty() {
        "/ws".to_string()
    } else {
        format!("{base_path}/ws")
    };
    url.set_path(&ws_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

async fn load_initial_state(client: &ApiClient) -> Result<App> {
    let account_summary = load_account_summary_best_effort(client).await;
    let response = client.list_tracks().await?;
    let mut app = App::new(response.items);
    if let Some(account_summary) = account_summary {
        app.apply_account_summary(account_summary);
    } else {
        app.clear_account_summary();
    }
    if let Some(track_id) = app.selected_track_id().map(ToOwned::to_owned) {
        let detail = client.get_track_detail(&track_id).await?;
        app.apply_track_detail(detail);
    }

    Ok(app)
}

async fn bootstrap_runtime_state(
    client: &ApiClient,
    ws_url: &str,
) -> (App, Option<tokio::sync::mpsc::Receiver<StreamEvent>>) {
    let mut app = match load_initial_state(client).await {
        Ok(app) => app,
        Err(error) => {
            let mut app = App::new(vec![]);
            app.set_status_message(format!("startup failed: {error}"));
            app.schedule_initial_load_retry(INITIAL_LOAD_RETRY_DELAY);
            return (app, None);
        }
    };

    let ws_receiver = match connect_ws(ws_url).await {
        Ok(receiver) => {
            finalize_websocket_connection(client, &mut app, "websocket connected").await;
            Some(receiver)
        }
        Err(error) => {
            app.set_status_message(format!("ws connect failed: {error}"));
            app.schedule_websocket_retry(WS_RECONNECT_DELAY);
            None
        }
    };

    (app, ws_receiver)
}

async fn run_loop(
    terminal: &mut AppTerminal,
    client: &ApiClient,
    ws_url: &str,
    app: &mut App,
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<StreamEvent>>,
) -> Result<()> {
    loop {
        maybe_load_initial_state(client, app).await;
        terminal.draw(|frame| views::render(app, frame))?;

        if app.should_quit {
            break;
        }

        let next_event =
            if event::poll(Duration::from_millis(50)).context("failed to poll input")? {
                Some(event::read().context("failed to read input")?)
            } else {
                None
            };

        if let Some(Event::Key(key)) = next_event {
            let action = handle_key_event(app, key);
            if let Err(error) = handle_action(client, app, action).await {
                app.set_status_message(format!("action failed: {error}"));
            }
        }

        process_ws_event(client, ws_url, app, ws_receiver).await;
    }

    Ok(())
}

async fn maybe_load_initial_state(client: &ApiClient, app: &mut App) {
    if !app.should_retry_initial_load() {
        return;
    }

    match load_initial_state(client).await {
        Ok(mut loaded_app) => {
            loaded_app.current_view = app.current_view;
            loaded_app.should_quit = app.should_quit;
            loaded_app.set_status_message("startup recovered");
            loaded_app.mark_initial_load_complete();
            *app = loaded_app;
        }
        Err(error) => {
            app.set_status_message(format!("startup failed: {error}"));
            app.schedule_initial_load_retry(INITIAL_LOAD_RETRY_DELAY);
        }
    }
}

async fn process_ws_event(
    client: &ApiClient,
    ws_url: &str,
    app: &mut App,
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<StreamEvent>>,
) {
    if ws_receiver.is_none() {
        maybe_reconnect_websocket(client, ws_url, app, ws_receiver).await;
        return;
    }

    let receiver = ws_receiver.as_mut().unwrap();
    let mut handled_event = false;

    loop {
        match receiver.try_recv() {
            Ok(event) => {
                handled_event = true;
                handle_ws_event(client, app, event).await;
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                if handled_event {
                    *ws_receiver = None;
                    app.schedule_websocket_retry(Duration::ZERO);
                } else {
                    reconnect_websocket(client, ws_url, app, ws_receiver).await;
                }
                break;
            }
        }
    }
}

async fn maybe_reconnect_websocket(
    client: &ApiClient,
    ws_url: &str,
    app: &mut App,
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<StreamEvent>>,
) {
    if !app.should_retry_websocket() {
        return;
    }

    connect_websocket(client, ws_url, app, ws_receiver, "websocket connected").await;
}

async fn handle_action(client: &ApiClient, app: &mut App, action: Action) -> Result<()> {
    match action {
        Action::None => Ok(()),
        Action::OpenSelectedInstance | Action::RefreshSelectedInstance => {
            refresh_selected_grid_detail(client, app).await?;
            app.show_instance_for_selected();
            Ok(())
        }
        Action::ToggleDiagnostics => {
            if app.toggle_debug_diagnostics() {
                refresh_selected_track_diagnostics(client, app).await?;
            }
            Ok(())
        }
        Action::SubmitCommand(command) => submit_selected_command(client, app, command).await,
    }
}

async fn submit_selected_command(
    client: &ApiClient,
    app: &mut App,
    command: CommandKind,
) -> Result<()> {
    let track_id = app
        .selected_track_id()
        .context("no instance selected for command")?
        .to_string();
    let response = client
        .submit_command(&track_id, command.as_track_command())
        .await
        .with_context(|| format!("failed to submit command for `{track_id}`"))?;
    if !response.accepted {
        anyhow::bail!(
            "command `{:?}` rejected for `{}`",
            response.command,
            response.track_id
        );
    }
    sync_projected_state(client, app).await?;
    app.set_status_message(format_command_response(&response));
    if app.current_view == View::Instance {
        app.show_instance_for_selected();
    }
    Ok(())
}

fn format_command_response(response: &TrackCommandAccepted) -> String {
    format!(
        "command `{:?}` accepted for `{}`",
        response.command, response.track_id
    )
    .to_ascii_lowercase()
}

async fn refresh_selected_grid_detail(client: &ApiClient, app: &mut App) -> Result<()> {
    let track_id = app
        .selected_track_id()
        .context("no instance selected")?
        .to_string();
    let detail = client.get_track_detail(&track_id).await?;
    app.apply_track_detail(detail);
    refresh_selected_track_diagnostics_best_effort(client, app).await;
    Ok(())
}

async fn load_account_summary_best_effort(client: &ApiClient) -> Option<AccountSummaryView> {
    match client.get_account_summary().await {
        Ok(summary) => Some(summary),
        Err(error) => {
            tracing::warn!("failed to refresh account summary: {error}");
            None
        }
    }
}

async fn refresh_selected_track_diagnostics(client: &ApiClient, app: &mut App) -> Result<()> {
    let track_id = app
        .selected_track_id()
        .context("no instance selected for diagnostics")?
        .to_string();
    let diagnostics = client.get_track_diagnostics(&track_id).await?;
    app.apply_track_diagnostics(diagnostics);
    Ok(())
}

async fn refresh_selected_track_diagnostics_best_effort(client: &ApiClient, app: &mut App) {
    if !app.debug_diagnostics_enabled() {
        app.clear_track_diagnostics();
        return;
    }

    if let Err(error) = refresh_selected_track_diagnostics(client, app).await {
        tracing::warn!("failed to refresh diagnostics: {error}");
        app.clear_track_diagnostics();
    }
}

async fn handle_ws_event(client: &ApiClient, app: &mut App, event: StreamEvent) {
    match event {
        StreamEvent::TrackListItemChanged { item, .. } => app.apply_track_list_item(item),
        StreamEvent::TrackDetailChanged { detail, .. } => {
            let selected_matches = app.selected_track_id() == Some(detail.identity.id.as_str());
            app.apply_track_detail(*detail);
            if selected_matches {
                refresh_selected_track_diagnostics_best_effort(client, app).await;
            }
        }
        StreamEvent::AccountSummaryChanged { summary } => app.apply_account_summary(summary),
    }
}

async fn reconnect_websocket(
    client: &ApiClient,
    ws_url: &str,
    app: &mut App,
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<StreamEvent>>,
) {
    app.set_status_message("websocket disconnected, reconnecting");
    connect_websocket(client, ws_url, app, ws_receiver, "websocket reconnected").await;
}

async fn connect_websocket(
    client: &ApiClient,
    ws_url: &str,
    app: &mut App,
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<StreamEvent>>,
    success_message: &str,
) {
    *ws_receiver = match connect_ws(ws_url).await {
        Ok(receiver) => Some(receiver),
        Err(error) => {
            app.set_status_message(format!("ws reconnect failed: {error}"));
            app.schedule_websocket_retry(WS_RECONNECT_DELAY);
            None
        }
    };
    if ws_receiver.is_some() {
        finalize_websocket_connection(client, app, success_message).await;
    }
}

async fn finalize_websocket_connection(client: &ApiClient, app: &mut App, success_message: &str) {
    app.mark_websocket_connected();
    match sync_projected_state(client, app).await {
        Ok(()) => app.set_status_message(success_message),
        Err(error) => {
            app.set_status_message(format!("{success_message}; refresh failed: {error}"));
            app.schedule_initial_load_retry(INITIAL_LOAD_RETRY_DELAY);
        }
    }
}

async fn sync_projected_state(client: &ApiClient, app: &mut App) -> Result<()> {
    let selected_track_id = app.selected_track_id().map(ToOwned::to_owned);
    let current_view = app.current_view;
    let should_quit = app.should_quit;
    let debug_diagnostics_enabled = app.debug_diagnostics_enabled();
    let should_load_diagnostics =
        debug_diagnostics_enabled && matches!(current_view, View::Instance);

    let response = client.list_tracks().await?;
    let mut refreshed = App::new(response.items);
    refreshed.set_debug_diagnostics_enabled(debug_diagnostics_enabled);
    if let Some(selected_track_id) = selected_track_id
        && let Some(index) = refreshed
            .grids
            .iter()
            .position(|grid| grid.id == selected_track_id)
    {
        refreshed.selected_index = index;
    }

    if let Some(track_id) = refreshed.selected_track_id().map(ToOwned::to_owned) {
        let detail = client.get_track_detail(&track_id).await?;
        refreshed.apply_track_detail(detail);
        if should_load_diagnostics {
            refresh_selected_track_diagnostics_best_effort(client, &mut refreshed).await;
        }
    }

    if let Some(account_summary) = load_account_summary_best_effort(client).await {
        refreshed.apply_account_summary(account_summary);
    } else {
        refreshed.clear_account_summary();
    }

    refreshed.current_view = current_view;
    refreshed.should_quit = should_quit;
    refreshed.mark_initial_load_complete();
    *app = refreshed;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio::time::{Duration, sleep};

    use super::{
        ApiClient, CommandKind, TracingDestination, View, bootstrap_runtime_state, derive_ws_url,
        format_command_response, handle_action, handle_ws_event, load_initial_state,
        maybe_load_initial_state, parse_tracing_destination, process_ws_event,
        submit_selected_command, sync_projected_state,
    };
    use crate::api_client::connect_ws;
    use crate::app::App;
    use crate::input::Action;
    use crate::protocol::{
        AccountSummaryView, ExecutionStateView, GridCommandType, GridStatus, RiskSignalView,
        StreamEvent, TrackCommandAccepted, TrackCommandRequest, TrackDetailView,
        TrackDiagnosticsView, TrackListItemView, TrackListResponse,
    };

    const BTC_GRID_ID: &str = "btc-core";
    const BTC_SYMBOL: &str = "BTCUSDT";
    const ETH_GRID_ID: &str = "eth-core";
    const ETH_SYMBOL: &str = "ETHUSDT";

    fn track_list_response() -> TrackListResponse {
        let mut response: TrackListResponse =
            serde_json::from_str(include_str!("../tests/fixtures/track_list_response.json"))
                .unwrap();
        let mut eth = response.items[0].clone();
        eth.id = ETH_GRID_ID.into();
        eth.instrument.symbol = ETH_SYMBOL.into();
        eth.reference_price = Some(2200.0);
        response.items.push(eth);
        response
    }

    fn detail_view(track_id: &str, symbol: &str) -> TrackDetailView {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../tests/fixtures/track_detail_view.json")).unwrap();
        detail.identity.id = track_id.into();
        detail.identity.instrument.symbol = symbol.into();
        detail.status.reference_price = Some(if symbol == ETH_SYMBOL { 2200.0 } else { 100.0 });
        detail.position.current_exposure = if symbol == ETH_SYMBOL { -1.0 } else { 2.0 };
        detail.execution.state = if symbol == ETH_SYMBOL {
            ExecutionStateView::Paused
        } else {
            ExecutionStateView::Open
        };
        detail.available_commands = if symbol == ETH_SYMBOL {
            vec![crate::protocol::GridCommandView {
                command: GridCommandType::Resume,
                enabled: true,
                disabled_reason: None,
            }]
        } else {
            vec![crate::protocol::GridCommandView {
                command: GridCommandType::Pause,
                enabled: true,
                disabled_reason: None,
            }]
        };
        detail
    }

    fn track_list_item_changed_event() -> StreamEvent {
        serde_json::from_str(include_str!(
            "../tests/fixtures/ws_track_list_item_changed.json"
        ))
        .unwrap()
    }

    fn track_detail_changed_event() -> StreamEvent {
        serde_json::from_str(include_str!(
            "../tests/fixtures/ws_track_detail_changed.json"
        ))
        .unwrap()
    }

    fn account_summary_view() -> AccountSummaryView {
        serde_json::from_str(include_str!(
            "../tests/fixtures/account_summary_view.json"
        ))
        .unwrap()
    }

    fn account_summary_changed_event() -> StreamEvent {
        serde_json::from_str(include_str!(
            "../tests/fixtures/ws_account_summary_changed.json"
        ))
        .unwrap()
    }

    #[derive(Clone)]
    struct ProjectionStubState {
        requests: Arc<Mutex<Vec<String>>>,
        account_summary_failures_left: Arc<Mutex<usize>>,
    }

    async fn list_projected_grids(
        State(state): State<ProjectionStubState>,
    ) -> Json<TrackListResponse> {
        state.requests.lock().await.push("/tracks".into());
        Json(track_list_response())
    }

    async fn get_projected_account_summary(
        State(state): State<ProjectionStubState>,
    ) -> Result<Json<AccountSummaryView>, StatusCode> {
        state.requests.lock().await.push("/account".into());
        let mut failures_left = state.account_summary_failures_left.lock().await;
        if *failures_left > 0 {
            *failures_left -= 1;
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        Ok(Json(account_summary_view()))
    }

    async fn get_projected_detail(
        Path(id): Path<String>,
        State(state): State<ProjectionStubState>,
    ) -> Json<TrackDetailView> {
        state.requests.lock().await.push(format!("/tracks/{id}"));
        Json(match id.as_str() {
            BTC_GRID_ID => detail_view(BTC_GRID_ID, BTC_SYMBOL),
            ETH_GRID_ID => detail_view(ETH_GRID_ID, ETH_SYMBOL),
            _ => panic!("unexpected grid id: {id}"),
        })
    }

    async fn get_projected_diagnostics(
        Path(id): Path<String>,
        State(state): State<ProjectionStubState>,
    ) -> Json<crate::protocol::TrackDiagnosticsView> {
        state
            .requests
            .lock()
            .await
            .push(format!("/debug/tracks/{id}/diagnostics"));
        Json(
            serde_json::from_str(include_str!(
                "../tests/fixtures/track_diagnostics_view.json"
            ))
            .unwrap(),
        )
    }

    async fn spawn_projection_stub_server() -> (ApiClient, ProjectionStubState) {
        let state = ProjectionStubState {
            requests: Arc::new(Mutex::new(vec![])),
            account_summary_failures_left: Arc::new(Mutex::new(0)),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/account", get(get_projected_account_summary))
            .route("/tracks", get(list_projected_grids))
            .route("/tracks/:id", get(get_projected_detail))
            .route(
                "/debug/tracks/:id/diagnostics",
                get(get_projected_diagnostics),
            )
            .with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (ApiClient::new(format!("http://{address}")), state)
    }

    async fn get_failing_projected_diagnostics(
        Path(id): Path<String>,
        State(state): State<ProjectionStubState>,
    ) -> (StatusCode, String) {
        state
            .requests
            .lock()
            .await
            .push(format!("/debug/tracks/{id}/diagnostics"));
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "diagnostics unavailable".to_string(),
        )
    }

    async fn spawn_projection_stub_server_with_failing_diagnostics()
    -> (ApiClient, ProjectionStubState) {
        let state = ProjectionStubState {
            requests: Arc::new(Mutex::new(vec![])),
            account_summary_failures_left: Arc::new(Mutex::new(0)),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/account", get(get_projected_account_summary))
            .route("/tracks", get(list_projected_grids))
            .route("/tracks/:id", get(get_projected_detail))
            .route(
                "/debug/tracks/:id/diagnostics",
                get(get_failing_projected_diagnostics),
            )
            .with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (ApiClient::new(format!("http://{address}")), state)
    }

    async fn spawn_projection_stub_server_with_failing_account_summary(
    ) -> (ApiClient, ProjectionStubState) {
        let state = ProjectionStubState {
            requests: Arc::new(Mutex::new(vec![])),
            account_summary_failures_left: Arc::new(Mutex::new(1)),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/account", get(get_projected_account_summary))
            .route("/tracks", get(list_projected_grids))
            .route("/tracks/:id", get(get_projected_detail))
            .route(
                "/debug/tracks/:id/diagnostics",
                get(get_projected_diagnostics),
            )
            .with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (ApiClient::new(format!("http://{address}")), state)
    }

    #[derive(Clone)]
    struct StubState {
        list: Arc<Mutex<TrackListResponse>>,
        details: Arc<Mutex<HashMap<String, TrackDetailView>>>,
    }

    fn list_item_from_detail(detail: &TrackDetailView) -> TrackListItemView {
        TrackListItemView {
            id: detail.identity.id.clone(),
            instrument: detail.identity.instrument.clone(),
            lifecycle: detail.status.lifecycle.clone(),
            reference_price: detail.status.reference_price,
            exposure: crate::protocol::ExposureSummaryView {
                current: detail.position.current_exposure,
                target: detail.position.target_exposure,
            },
            execution: crate::protocol::ExecutionBadgeView {
                state: detail.execution.state,
                execution_status: detail.execution.execution_status,
                active_slot_count: detail.execution.active_slot_count,
            },
            statistics: crate::protocol::TrackListStatisticsView {
                total_pnl: detail.statistics.total_pnl,
            },
        }
    }

    fn stub_state() -> StubState {
        let btc = detail_view(BTC_GRID_ID, BTC_SYMBOL);
        let eth = detail_view(ETH_GRID_ID, ETH_SYMBOL);
        StubState {
            list: Arc::new(Mutex::new(TrackListResponse {
                items: vec![list_item_from_detail(&btc), list_item_from_detail(&eth)],
            })),
            details: Arc::new(Mutex::new(HashMap::from([
                (BTC_GRID_ID.to_string(), btc),
                (ETH_GRID_ID.to_string(), eth),
            ]))),
        }
    }

    async fn list_tracks(State(state): State<StubState>) -> Json<TrackListResponse> {
        Json(state.list.lock().await.clone())
    }

    async fn get_account_summary() -> Json<AccountSummaryView> {
        Json(account_summary_view())
    }

    async fn get_track_detail(
        Path(id): Path<String>,
        State(state): State<StubState>,
    ) -> Json<TrackDetailView> {
        Json(state.details.lock().await.get(&id).unwrap().clone())
    }

    async fn submit_command(
        Path(id): Path<String>,
        State(state): State<StubState>,
        Json(command): Json<TrackCommandRequest>,
    ) -> Json<TrackCommandAccepted> {
        let mut details = state.details.lock().await;
        let detail = details.get_mut(&id).unwrap();
        match command.command {
            GridCommandType::Pause => {
                detail.status.lifecycle.status = GridStatus::Paused;
                detail.execution.state = ExecutionStateView::Paused;
            }
            GridCommandType::Resume => {
                detail.status.lifecycle.status = GridStatus::Active;
                detail.execution.state = ExecutionStateView::Open;
            }
            _ => {}
        }
        let list_item = list_item_from_detail(detail);
        let mut list = state.list.lock().await;
        if let Some(item) = list.items.iter_mut().find(|item| item.id == id) {
            *item = list_item;
        }

        Json(TrackCommandAccepted {
            track_id: id,
            command: command.command,
            accepted: true,
        })
    }

    async fn ws_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(handle_ws_socket)
    }

    async fn handle_ws_socket(mut socket: WebSocket) {
        let payload = serde_json::to_string(&track_detail_changed_event()).unwrap();
        socket.send(AxumMessage::Text(payload)).await.unwrap();
    }

    async fn silent_ws_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(handle_silent_ws_socket)
    }

    async fn handle_silent_ws_socket(mut socket: WebSocket) {
        while socket.recv().await.is_some() {}
    }

    async fn exchange_ws_handler(Path(stream): Path<String>, ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(move |socket| handle_exchange_socket(socket, stream))
    }

    async fn handle_exchange_socket(mut socket: WebSocket, stream: String) {
        if !stream.contains('@') {
            while socket.recv().await.is_some() {}
            return;
        }

        let symbol = stream
            .split('@')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let prices = if symbol == "ETHUSDT" {
            ["2300.00", "2200.00", "2400.00", "2250.00", "2350.00"]
        } else {
            ["95.00", "100.00", "105.00", "97.50", "102.50"]
        };

        for price in prices {
            let payload = format!(
                r#"{{"e":"markPriceUpdate","E":1700000000000,"s":"{symbol}","p":"{price}","i":"{price}"}}"#
            );
            if socket.send(AxumMessage::Text(payload)).await.is_err() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    async fn exchange_info() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "symbols": [
                exchange_info_symbol("BTCUSDT"),
                exchange_info_symbol("ETHUSDT")
            ]
        }))
    }

    async fn server_time() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "serverTime": 1_700_000_000_000_i64
        }))
    }

    fn exchange_info_symbol(symbol: &str) -> serde_json::Value {
        serde_json::json!({
            "symbol": symbol,
            "filters": [
                {
                    "filterType": "PRICE_FILTER",
                    "tickSize": "0.1"
                },
                {
                    "filterType": "LOT_SIZE",
                    "minQty": "0.001",
                    "stepSize": "0.001"
                },
                {
                    "filterType": "MIN_NOTIONAL",
                    "notional": "5.0"
                }
            ]
        })
    }

    async fn position_risk(
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        let symbol = params
            .get("symbol")
            .cloned()
            .unwrap_or_else(|| "BTCUSDT".to_string());

        Json(serde_json::json!([
            {
                "symbol": symbol,
                "positionAmt": "0.0",
                "entryPrice": "0.0",
                "unRealizedProfit": "0.0"
            }
        ]))
    }

    async fn account_information() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "availableBalance": "1000.0",
            "totalWalletBalance": "1200.0",
            "positions": [
                { "symbol": "BTCUSDT", "leverage": "20" },
                { "symbol": "ETHUSDT", "leverage": "10" }
            ]
        }))
    }

    async fn open_orders() -> Json<serde_json::Value> {
        Json(serde_json::json!([]))
    }

    async fn create_order(
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        let client_order_id = params
            .get("newClientOrderId")
            .cloned()
            .unwrap_or_else(|| "grid-order-test".to_string());

        Json(serde_json::json!({
            "orderId": 1001,
            "clientOrderId": client_order_id,
            "status": "NEW"
        }))
    }

    async fn cancel_order(
        Query(params): Query<HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        let order_id = params
            .get("orderId")
            .cloned()
            .unwrap_or_else(|| "1001".to_string());

        Json(serde_json::json!({
            "orderId": order_id.parse::<u64>().unwrap_or(1001),
            "clientOrderId": "grid-order-test",
            "status": "CANCELED"
        }))
    }

    async fn cancel_all_orders() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "code": 200,
            "msg": "cancel all open orders accepted"
        }))
    }

    async fn create_listen_key() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "listenKey": "listen-key-1"
        }))
    }

    async fn keepalive_listen_key() -> Json<serde_json::Value> {
        Json(serde_json::json!({}))
    }

    async fn spawn_stub_server() -> (ApiClient, String, StubState) {
        let state = stub_state();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/account", get(get_account_summary))
            .route("/tracks", get(list_tracks))
            .route("/tracks/:id", get(get_track_detail))
            .route("/tracks/:id/commands", post(submit_command))
            .route("/ws", get(ws_handler))
            .with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (
            ApiClient::new(format!("http://{address}")),
            format!("ws://{address}/ws"),
            state,
        )
    }

    async fn spawn_stub_server_on(
        address: std::net::SocketAddr,
    ) -> (ApiClient, String, StubState, tokio::task::JoinHandle<()>) {
        let state = stub_state();
        let listener = TcpListener::bind(address).await.unwrap();
        let app = Router::new()
            .route("/account", get(get_account_summary))
            .route("/tracks", get(list_tracks))
            .route("/tracks/:id", get(get_track_detail))
            .route("/tracks/:id/commands", post(submit_command))
            .route("/ws", get(ws_handler))
            .with_state(state.clone());

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (
            ApiClient::new(format!("http://{address}")),
            format!("ws://{address}/ws"),
            state,
            handle,
        )
    }

    #[test]
    fn derives_ws_url_from_http_base_url() {
        let url = derive_ws_url("http://127.0.0.1:8000").unwrap();

        assert_eq!(url, "ws://127.0.0.1:8000/ws");
    }

    #[test]
    fn tracing_destination_defaults_to_disabled() {
        assert_eq!(
            parse_tracing_destination(None),
            TracingDestination::Disabled
        );
        assert_eq!(
            parse_tracing_destination(Some("   ")),
            TracingDestination::Disabled
        );
    }

    #[test]
    fn tracing_destination_accepts_stderr_keyword() {
        assert_eq!(
            parse_tracing_destination(Some("stderr")),
            TracingDestination::Stderr
        );
        assert_eq!(
            parse_tracing_destination(Some(" STDERR ")),
            TracingDestination::Stderr
        );
    }

    #[test]
    fn tracing_destination_treats_other_values_as_log_path() {
        assert_eq!(
            parse_tracing_destination(Some("/tmp/poise-tui.log")),
            TracingDestination::File(PathBuf::from("/tmp/poise-tui.log"))
        );
    }

    #[test]
    fn derives_ws_url_from_base_url_with_path_prefix() {
        let url = derive_ws_url("https://example.com/grid/api").unwrap();

        assert_eq!(url, "wss://example.com/grid/api/ws");
    }

    #[test]
    fn rejects_unsupported_base_url_scheme() {
        let error = derive_ws_url("ftp://127.0.0.1:8000").unwrap_err();

        assert!(error.to_string().contains("unsupported base url scheme"));
    }

    #[test]
    fn formats_command_response_message() {
        let text = format_command_response(&TrackCommandAccepted {
            track_id: BTC_GRID_ID.into(),
            command: crate::protocol::GridCommandType::Pause,
            accepted: true,
        });

        assert_eq!(text, "command `pause` accepted for `btc-core`");
    }

    #[tokio::test]
    async fn load_initial_state_fetches_account_before_tracks() {
        let (client, state) = spawn_projection_stub_server().await;

        let app = load_initial_state(&client).await.unwrap();

        assert_eq!(app.account_summary.as_ref().unwrap().equity, Some(12_500.0));
        assert_eq!(app.grids.len(), 2);
        assert_eq!(app.current_track.as_ref().unwrap().identity.id, BTC_GRID_ID);
        assert_eq!(
            state.requests.lock().await.clone(),
            vec![
                "/account".to_string(),
                "/tracks".to_string(),
                format!("/tracks/{BTC_GRID_ID}")
            ]
        );
    }

    #[tokio::test]
    async fn load_initial_state_keeps_tracks_when_account_summary_request_fails() {
        let (client, state) = spawn_projection_stub_server_with_failing_account_summary().await;

        let app = load_initial_state(&client).await.unwrap();

        assert!(app.account_summary.is_none());
        assert_eq!(app.grids.len(), 2);
        assert_eq!(app.current_track.as_ref().unwrap().identity.id, BTC_GRID_ID);
        assert_eq!(
            state.requests.lock().await.clone(),
            vec![
                "/account".to_string(),
                "/tracks".to_string(),
                format!("/tracks/{BTC_GRID_ID}")
            ]
        );
    }

    #[tokio::test]
    async fn diagnostics_are_requested_only_after_debug_toggle() {
        let (client, state) = spawn_projection_stub_server().await;
        let mut app = load_initial_state(&client).await.unwrap();
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        assert_eq!(
            state.requests.lock().await.clone(),
            vec![
                "/account".to_string(),
                "/tracks".to_string(),
                format!("/tracks/{BTC_GRID_ID}")
            ]
        );

        handle_action(&client, &mut app, Action::ToggleDiagnostics)
            .await
            .unwrap();

        assert_eq!(
            state.requests.lock().await.clone(),
            vec![
                "/account".to_string(),
                "/tracks".to_string(),
                format!("/tracks/{BTC_GRID_ID}"),
                format!("/debug/tracks/{BTC_GRID_ID}/diagnostics"),
            ]
        );
    }

    #[tokio::test]
    async fn sync_projected_state_keeps_stable_detail_when_diagnostics_request_fails() {
        let (client, state) = spawn_projection_stub_server_with_failing_diagnostics().await;
        let mut app = load_initial_state(&client).await.unwrap();
        app.current_view = View::Instance;
        app.show_instance_for_selected();
        app.set_debug_diagnostics_enabled(true);

        sync_projected_state(&client, &mut app).await.unwrap();

        assert_eq!(app.current_track.as_ref().unwrap().identity.id, BTC_GRID_ID);
        assert!(app.current_track_diagnostics().is_none());
        assert!(
            state
                .requests
                .lock()
                .await
                .contains(&format!("/debug/tracks/{BTC_GRID_ID}/diagnostics"))
        );
    }

    #[tokio::test]
    async fn sync_projected_state_keeps_tracks_when_account_summary_request_fails() {
        let (client, state) = spawn_projection_stub_server().await;
        let mut app = load_initial_state(&client).await.unwrap();
        assert!(app.account_summary.is_some());
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        *state.account_summary_failures_left.lock().await = 1;

        sync_projected_state(&client, &mut app).await.unwrap();

        assert!(app.account_summary.is_none());
        assert_eq!(app.grids.len(), 2);
        assert_eq!(app.current_track.as_ref().unwrap().identity.id, BTC_GRID_ID);
        assert_eq!(
            state.requests.lock().await.clone(),
            vec![
                "/account".to_string(),
                "/tracks".to_string(),
                format!("/tracks/{BTC_GRID_ID}"),
                "/tracks".to_string(),
                format!("/tracks/{BTC_GRID_ID}"),
                "/account".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn handle_ws_event_refreshes_diagnostics_for_selected_track_in_debug_mode() {
        let (client, _) = spawn_projection_stub_server().await;
        let mut app = App::new(track_list_response().items);
        app.current_view = View::Instance;
        app.show_instance_for_selected();
        app.set_debug_diagnostics_enabled(true);
        app.apply_track_detail(detail_view(BTC_GRID_ID, BTC_SYMBOL));
        let mut stale_diagnostics: TrackDiagnosticsView = serde_json::from_str(include_str!(
            "../tests/fixtures/track_diagnostics_view.json"
        ))
        .unwrap();
        stale_diagnostics.items[0].message = "stale diagnostics".into();
        app.apply_track_diagnostics(stale_diagnostics);

        handle_ws_event(&client, &mut app, track_detail_changed_event()).await;

        assert_eq!(
            app.current_track.as_ref().unwrap().status.reference_price,
            Some(101.5)
        );
        assert_eq!(app.current_track_diagnostics().unwrap().items.len(), 1);
        assert!(
            app.current_track_diagnostics().unwrap().items[0]
                .message
                .contains("target exposure")
        );
    }

    #[tokio::test]
    async fn handle_ws_event_applies_projected_updates_without_refetch() {
        let mut app = App::new(track_list_response().items);
        app.current_view = View::Instance;
        app.show_instance_for_selected();
        app.apply_track_detail(detail_view(BTC_GRID_ID, BTC_SYMBOL));

        let (client, _) = spawn_projection_stub_server().await;
        handle_ws_event(&client, &mut app, track_list_item_changed_event()).await;
        handle_ws_event(&client, &mut app, track_detail_changed_event()).await;

        assert_eq!(app.grids[0].reference_price, Some(101.4));
        assert_eq!(
            app.current_track.as_ref().unwrap().status.reference_price,
            Some(101.5)
        );
        assert!(matches!(
            track_detail_changed_event(),
            StreamEvent::TrackDetailChanged { .. }
        ));
    }

    #[tokio::test]
    async fn handle_ws_event_applies_account_summary_changed() {
        let (client, _) = spawn_projection_stub_server().await;
        let mut app = App::new(track_list_response().items);

        handle_ws_event(&client, &mut app, account_summary_changed_event()).await;

        assert_eq!(app.account_summary.as_ref().unwrap().equity, Some(12_420.0));
        assert_eq!(
            app.account_summary.as_ref().unwrap().risk_signal,
            RiskSignalView::Attention
        );
        assert_eq!(
            app.account_summary.as_ref().unwrap().reason.as_deref(),
            Some("available 18.0%")
        );
    }

    #[tokio::test]
    async fn loads_initial_state_with_selected_detail() {
        let (client, _, _) = spawn_stub_server().await;

        let app = load_initial_state(&client).await.unwrap();

        assert_eq!(app.grids.len(), 2);
        assert_eq!(app.current_track.as_ref().unwrap().identity.id, BTC_GRID_ID);
        assert_eq!(
            app.current_track
                .as_ref()
                .unwrap()
                .position
                .current_exposure,
            2.0
        );
    }

    #[tokio::test]
    async fn process_ws_event_drains_all_pending_events_in_one_iteration() {
        let (client, ws_url, _) = spawn_stub_server().await;
        let mut app = load_initial_state(&client).await.unwrap();
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        let mut first = detail_view(BTC_GRID_ID, BTC_SYMBOL);
        first.status.reference_price = Some(101.0);
        let mut second = detail_view(BTC_GRID_ID, BTC_SYMBOL);
        second.status.reference_price = Some(102.0);
        second.position.current_exposure = 3.0;

        sender
            .send(StreamEvent::TrackDetailChanged {
                track_id: BTC_GRID_ID.into(),
                detail: Box::new(first),
            })
            .await
            .unwrap();
        sender
            .send(StreamEvent::TrackDetailChanged {
                track_id: BTC_GRID_ID.into(),
                detail: Box::new(second),
            })
            .await
            .unwrap();

        let mut ws_receiver = Some(receiver);
        process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;

        assert_eq!(
            app.current_track.as_ref().unwrap().status.reference_price,
            Some(102.0)
        );
        assert_eq!(
            app.current_track
                .as_ref()
                .unwrap()
                .position
                .current_exposure,
            3.0
        );
        assert!(matches!(
            ws_receiver.as_mut().unwrap().try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        drop(sender);
    }

    #[tokio::test]
    async fn websocket_event_applies_projected_detail() {
        let (client, _, _) = spawn_stub_server().await;
        let mut app = load_initial_state(&client).await.unwrap();
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        handle_ws_event(&client, &mut app, track_detail_changed_event()).await;

        assert_eq!(
            app.current_track.as_ref().unwrap().status.reference_price,
            Some(101.5)
        );
    }

    #[tokio::test]
    async fn submits_pause_command_and_refreshes_detail() {
        let (client, _, _) = spawn_stub_server().await;
        let mut app = load_initial_state(&client).await.unwrap();
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        submit_selected_command(&client, &mut app, CommandKind::Pause)
            .await
            .unwrap();

        assert_eq!(
            app.current_track_detail().unwrap().status.lifecycle.status,
            GridStatus::Paused
        );
        assert!(
            app.status_message()
                .unwrap()
                .contains("command `pause` accepted")
        );
    }

    #[tokio::test]
    async fn submits_pause_command_refreshes_dashboard_when_websocket_is_unavailable() {
        let (client, _, _) = spawn_stub_server().await;
        let mut app = load_initial_state(&client).await.unwrap();
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        assert_eq!(app.grids[0].lifecycle.status, GridStatus::Active);

        submit_selected_command(&client, &mut app, CommandKind::Pause)
            .await
            .unwrap();

        assert_eq!(
            app.current_track_detail().unwrap().status.lifecycle.status,
            GridStatus::Paused
        );
        assert_eq!(app.grids[0].lifecycle.status, GridStatus::Paused);
        assert_eq!(app.grids[0].execution.state, ExecutionStateView::Paused);
    }

    #[tokio::test]
    async fn startup_failure_still_returns_a_tui_state() {
        let client = ApiClient::new("http://127.0.0.1:1");

        let (app, ws_receiver) = bootstrap_runtime_state(&client, "ws://127.0.0.1:1/ws").await;

        assert!(app.grids.is_empty());
        assert!(app.status_message().unwrap().contains("startup failed"));
        assert!(ws_receiver.is_none());
    }

    #[tokio::test]
    async fn retries_initial_http_load_after_startup_failure() {
        let bind_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = bind_listener.local_addr().unwrap();
        drop(bind_listener);
        let client = ApiClient::new(format!("http://{bind_address}"));
        let ws_url = format!("ws://{bind_address}/ws");

        let (mut app, mut ws_receiver) = bootstrap_runtime_state(&client, &ws_url).await;
        assert!(app.grids.is_empty());
        assert!(app.status_message().unwrap().contains("startup failed"));

        let (_, _, _, server) = spawn_stub_server_on(bind_address).await;

        for _ in 0..20 {
            maybe_load_initial_state(&client, &mut app).await;
            process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;
            if app.grids.len() == 2 && app.current_track.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert_eq!(app.grids.len(), 2);
        assert_eq!(app.current_track.as_ref().unwrap().identity.id, BTC_GRID_ID);

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn retries_websocket_after_startup_failure() {
        let (client, _, _) = spawn_stub_server().await;
        let bind_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = bind_listener.local_addr().unwrap();
        drop(bind_listener);
        let ws_url = format!("ws://{bind_address}/ws");

        let (mut app, mut ws_receiver) = bootstrap_runtime_state(&client, &ws_url).await;
        assert!(app.status_message().unwrap().contains("ws connect failed"));
        assert!(ws_receiver.is_none());

        let ws_server = spawn_ws_server_on(bind_address).await;

        for _ in 0..20 {
            process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;
            if app
                .current_track
                .as_ref()
                .and_then(|detail| detail.status.reference_price)
                == Some(101.5)
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert_eq!(
            app.current_track.as_ref().unwrap().status.reference_price,
            Some(101.5)
        );

        ws_server.abort();
        let _ = ws_server.await;
    }

    #[tokio::test]
    async fn reconnect_resyncs_http_state_even_when_websocket_pushes_nothing() {
        let (client, _, state) = spawn_stub_server().await;
        let bind_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = bind_listener.local_addr().unwrap();
        drop(bind_listener);
        let ws_url = format!("ws://{bind_address}/ws");

        let (mut app, mut ws_receiver) = bootstrap_runtime_state(&client, &ws_url).await;
        assert!(app.status_message().unwrap().contains("ws connect failed"));
        assert_eq!(
            app.current_track
                .as_ref()
                .and_then(|detail| detail.status.reference_price),
            Some(100.0)
        );

        let mut updated = detail_view(BTC_GRID_ID, BTC_SYMBOL);
        updated.status.reference_price = Some(111.5);
        updated.position.current_exposure = 4.5;
        replace_track_detail(&state, updated).await;

        let ws_server = spawn_silent_ws_server_on(bind_address).await;

        for _ in 0..20 {
            process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;
            if app
                .current_track
                .as_ref()
                .and_then(|detail| detail.status.reference_price)
                == Some(111.5)
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert!(ws_receiver.is_some());
        assert_eq!(
            app.current_track
                .as_ref()
                .and_then(|detail| detail.status.reference_price),
            Some(111.5)
        );
        assert_eq!(app.grids[0].reference_price, Some(111.5));

        ws_server.abort();
        let _ = ws_server.await;
    }

    #[tokio::test]
    async fn reconnects_when_websocket_receiver_disconnects() {
        let (client, ws_url, _) = spawn_stub_server().await;
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(sender);
        let mut app = App::new(vec![]);
        let mut ws_receiver = Some(receiver);

        process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;

        let event = ws_receiver.as_mut().unwrap().recv().await.unwrap();
        assert!(matches!(
            event,
            StreamEvent::TrackDetailChanged { ref track_id, .. } if track_id == BTC_GRID_ID
        ));
        assert_eq!(app.status_message(), Some("websocket reconnected"));
    }

    #[tokio::test]
    async fn retries_websocket_after_failed_reconnect() {
        let (client, _, _) = spawn_stub_server().await;
        let bind_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = bind_listener.local_addr().unwrap();
        drop(bind_listener);
        let ws_url = format!("ws://{bind_address}/ws");
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(sender);
        let mut app = load_initial_state(&client).await.unwrap();
        let mut ws_receiver = Some(receiver);

        process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;
        assert!(
            app.status_message()
                .unwrap()
                .contains("ws reconnect failed")
        );
        assert!(ws_receiver.is_none());

        let ws_server = spawn_ws_server_on(bind_address).await;

        for _ in 0..20 {
            process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;
            if app
                .current_track
                .as_ref()
                .and_then(|detail| detail.status.reference_price)
                == Some(101.5)
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert_eq!(
            app.current_track.as_ref().unwrap().status.reference_price,
            Some(101.5)
        );

        ws_server.abort();
        let _ = ws_server.await;
    }

    struct FakeExchangeServer {
        rest_base_url: String,
        ws_base_url: String,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for FakeExchangeServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn spawn_fake_exchange_server() -> FakeExchangeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/ws/:stream", get(exchange_ws_handler))
            .route("/fapi/v1/time", get(server_time))
            .route("/fapi/v1/exchangeInfo", get(exchange_info))
            .route("/fapi/v2/account", get(account_information))
            .route("/fapi/v2/positionRisk", get(position_risk))
            .route("/fapi/v1/openOrders", get(open_orders))
            .route("/fapi/v1/order", post(create_order).delete(cancel_order))
            .route(
                "/fapi/v1/allOpenOrders",
                axum::routing::delete(cancel_all_orders),
            )
            .route(
                "/fapi/v1/listenKey",
                post(create_listen_key).put(keepalive_listen_key),
            );

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        FakeExchangeServer {
            rest_base_url: format!("http://{address}"),
            ws_base_url: format!("ws://{address}"),
            handle,
        }
    }

    async fn spawn_ws_server_on(address: std::net::SocketAddr) -> tokio::task::JoinHandle<()> {
        let listener = TcpListener::bind(address).await.unwrap();
        let app = Router::new().route("/ws", get(ws_handler));

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })
    }

    async fn spawn_silent_ws_server_on(
        address: std::net::SocketAddr,
    ) -> tokio::task::JoinHandle<()> {
        let listener = TcpListener::bind(address).await.unwrap();
        let app = Router::new().route("/ws", get(silent_ws_handler));

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })
    }

    async fn replace_track_detail(state: &StubState, detail: TrackDetailView) {
        let track_id = detail.identity.id.clone();
        state
            .details
            .lock()
            .await
            .insert(track_id.clone(), detail.clone());
        let list_item = list_item_from_detail(&detail);
        let mut list = state.list.lock().await;
        if let Some(item) = list.items.iter_mut().find(|item| item.id == track_id) {
            *item = list_item;
        }
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn grid_tui_binary_path() -> PathBuf {
        let mut path = workspace_root().join("target").join("debug");
        path.push(if cfg!(windows) {
            "poise-tui.exe"
        } else {
            "poise-tui"
        });
        path
    }

    fn grid_server_binary_path() -> PathBuf {
        let mut path = workspace_root().join("target").join("debug");
        path.push(if cfg!(windows) {
            "poise-server.exe"
        } else {
            "poise-server"
        });
        path
    }

    fn ensure_grid_server_binary() -> PathBuf {
        let path = grid_server_binary_path();
        let status = Command::new("cargo")
            .arg("build")
            .arg("-p")
            .arg("poise-server")
            .current_dir(workspace_root())
            .status()
            .unwrap();
        assert!(status.success());
        path
    }

    fn ensure_grid_tui_binary() -> PathBuf {
        let path = grid_tui_binary_path();
        let status = Command::new("cargo")
            .arg("build")
            .arg("-p")
            .arg("poise-tui")
            .current_dir(workspace_root())
            .status()
            .unwrap();
        assert!(status.success());
        path
    }

    struct TmuxSession {
        name: String,
    }

    impl TmuxSession {
        fn start(command: &str) -> Self {
            let name = format!(
                "poise-tui-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let status = Command::new("tmux")
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    &name,
                    "-x",
                    "120",
                    "-y",
                    "40",
                    command,
                ])
                .status()
                .unwrap();
            assert!(status.success());
            Self { name }
        }

        fn capture_pane(&self) -> String {
            let output = Command::new("tmux")
                .args(["capture-pane", "-p", "-t", &self.name])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        }

        fn send_keys(&self, keys: &[&str]) {
            let status = Command::new("tmux")
                .arg("send-keys")
                .arg("-t")
                .arg(&self.name)
                .args(keys)
                .status()
                .unwrap();
            assert!(status.success());
        }

        fn is_alive(&self) -> bool {
            Command::new("tmux")
                .args(["has-session", "-t", &self.name])
                .output()
                .unwrap()
                .status
                .success()
        }
    }

    async fn wait_for_pane_text(session: &TmuxSession, needle: &str) -> String {
        for _ in 0..40 {
            let pane = session.capture_pane();
            if pane.contains(needle) {
                return pane;
            }
            sleep(Duration::from_millis(100)).await;
        }

        session.capture_pane()
    }

    impl Drop for TmuxSession {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.name])
                .output();
        }
    }

    fn drain_child_pipe<T: Read>(pipe: &mut Option<T>) -> String {
        let Some(mut pipe) = pipe.take() else {
            return String::new();
        };
        let mut output = String::new();
        let _ = pipe.read_to_string(&mut output);
        output
    }

    async fn wait_for_http_ready(base_url: &str, child: &mut Child) {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .unwrap();
        for _ in 0..50 {
            if let Some(status) = child.try_wait().unwrap() {
                let stdout = drain_child_pipe(&mut child.stdout);
                let stderr = drain_child_pipe(&mut child.stderr);
                panic!(
                    "server exited before becoming ready: status={status} stdout={stdout:?} stderr={stderr:?}"
                );
            }
            let Ok(response) = client.get(format!("{base_url}/tracks")).send().await else {
                sleep(Duration::from_millis(100)).await;
                continue;
            };
            if response.status().is_success() {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }

        let stdout = drain_child_pipe(&mut child.stdout);
        let stderr = drain_child_pipe(&mut child.stderr);
        panic!("server did not become ready; stdout={stdout:?} stderr={stderr:?}");
    }

    async fn wait_for_detail_price(client: &ApiClient, id: &str) {
        for _ in 0..50 {
            let detail = client.get_track_detail(id).await.unwrap();
            if detail.status.reference_price.is_some() {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }

        panic!("detail `{id}` never received market price");
    }

    #[tokio::test]
    async fn real_server_protocol_integration_covers_list_switch_and_ws_updates() {
        let exchange = spawn_fake_exchange_server().await;
        let server_binary = ensure_grid_server_binary();
        let temp_dir = tempfile::tempdir().unwrap();
        let bind_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = bind_listener.local_addr().unwrap();
        drop(bind_listener);
        let config_path = temp_dir.path().join("poise-server.toml");
        fs::write(
            &config_path,
            format!(
                r#"
environment = "test"
bind_address = "{bind_address}"

[exchange]
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0

[[tracks]]
track_id = "eth-core"
venue = "binance"
symbol = "ETHUSDT"
lower_price = 2000.0
upper_price = 2600.0
long_exposure_units = 5.0
short_exposure_units = 4.0
notional_per_unit = 2000.0
shape_family = "concave"
out_of_band_policy = "hold"
"#,
            ),
        )
        .unwrap();

        let mut server = Command::new(server_binary)
            .arg("--config")
            .arg(&config_path)
            .env("POISE_TEST_BINANCE_REST_BASE_URL", &exchange.rest_base_url)
            .env("POISE_TEST_BINANCE_WS_BASE_URL", &exchange.ws_base_url)
            .current_dir(temp_dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let base_url = format!("http://{bind_address}");
        let ws_url = format!("ws://{bind_address}/ws");
        wait_for_http_ready(&base_url, &mut server).await;
        let mut ws_receiver = Some(connect_ws(&ws_url).await.unwrap());

        let client = ApiClient::new(base_url);
        wait_for_detail_price(&client, BTC_GRID_ID).await;
        wait_for_detail_price(&client, ETH_GRID_ID).await;

        let mut app = load_initial_state(&client).await.unwrap();
        assert_eq!(app.grids.len(), 2);
        assert!(app.grids.iter().all(|grid| grid.reference_price.is_some()));

        let action = crate::input::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        handle_action(&client, &mut app, action).await.unwrap();
        assert_eq!(app.current_track.as_ref().unwrap().identity.id, BTC_GRID_ID);

        let action = crate::input::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
        );
        handle_action(&client, &mut app, action).await.unwrap();
        assert_eq!(app.current_track.as_ref().unwrap().identity.id, ETH_GRID_ID);

        for _ in 0..30 {
            let before = app
                .current_track
                .as_ref()
                .and_then(|detail| detail.status.reference_price);
            process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;
            if app
                .current_track
                .as_ref()
                .and_then(|detail| detail.status.reference_price)
                != before
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert!(
            app.current_track
                .as_ref()
                .unwrap()
                .status
                .reference_price
                .is_some()
        );

        let _ = server.kill();
        let _ = server.wait();
    }

    #[tokio::test]
    async fn real_server_starts_with_loopback_exchange_even_when_proxy_env_is_set() {
        let exchange = spawn_fake_exchange_server().await;
        let server_binary = ensure_grid_server_binary();
        let temp_dir = tempfile::tempdir().unwrap();
        let bind_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = bind_listener.local_addr().unwrap();
        drop(bind_listener);
        let config_path = temp_dir.path().join("poise-server.toml");
        fs::write(
            &config_path,
            format!(
                r#"
environment = "test"
bind_address = "{bind_address}"

[exchange]
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
            ),
        )
        .unwrap();

        let mut server = Command::new(server_binary)
            .arg("--config")
            .arg(&config_path)
            .env("POISE_TEST_BINANCE_REST_BASE_URL", &exchange.rest_base_url)
            .env("POISE_TEST_BINANCE_WS_BASE_URL", &exchange.ws_base_url)
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .current_dir(temp_dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let base_url = format!("http://{bind_address}");
        wait_for_http_ready(&base_url, &mut server).await;

        let _ = server.kill();
        let _ = server.wait();
    }

    #[tokio::test]
    async fn real_server_and_tui_binary_end_to_end_renders_and_exits() {
        let exchange = spawn_fake_exchange_server().await;
        let server_binary = ensure_grid_server_binary();
        let tui_binary = ensure_grid_tui_binary();
        let temp_dir = tempfile::tempdir().unwrap();
        let bind_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = bind_listener.local_addr().unwrap();
        drop(bind_listener);
        let config_path = temp_dir.path().join("poise-server.toml");
        fs::write(
            &config_path,
            format!(
                r#"
environment = "test"
bind_address = "{bind_address}"

[exchange]
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0

[[tracks]]
track_id = "eth-core"
venue = "binance"
symbol = "ETHUSDT"
lower_price = 2000.0
upper_price = 2600.0
long_exposure_units = 5.0
short_exposure_units = 4.0
notional_per_unit = 2000.0
shape_family = "concave"
out_of_band_policy = "hold"
"#,
            ),
        )
        .unwrap();

        let mut server = Command::new(server_binary)
            .arg("--config")
            .arg(&config_path)
            .env("POISE_TEST_BINANCE_REST_BASE_URL", &exchange.rest_base_url)
            .env("POISE_TEST_BINANCE_WS_BASE_URL", &exchange.ws_base_url)
            .current_dir(temp_dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let base_url = format!("http://{bind_address}");
        let ws_url = format!("ws://{bind_address}/ws");
        wait_for_http_ready(&base_url, &mut server).await;
        let session = TmuxSession::start(&format!(
            "env POISE_BASE_URL={base_url} POISE_TUI_WS_URL={ws_url} {}",
            tui_binary.display()
        ));

        let dashboard = wait_for_pane_text(&session, "BTCUSDT").await;
        assert!(dashboard.contains("BTCUSDT"), "dashboard:\n{dashboard}");
        assert!(dashboard.contains("ETHUSDT"), "dashboard:\n{dashboard}");

        session.send_keys(&["Enter"]);
        let btc_view = wait_for_pane_text(&session, "Overview").await;
        assert!(btc_view.contains("Overview"), "btc view:\n{btc_view}");
        assert!(btc_view.contains("BTCUSDT"), "btc view:\n{btc_view}");

        session.send_keys(&["]"]);
        let eth_view = wait_for_pane_text(&session, "ETHUSDT").await;
        assert!(eth_view.contains("ETHUSDT"), "eth view:\n{eth_view}");
        assert!(
            eth_view.contains("out of band policy: hold"),
            "eth view:\n{eth_view}"
        );

        let event_view = wait_for_pane_text(&session, "Activity").await;
        assert!(event_view.contains("Activity"), "event view:\n{event_view}");

        session.send_keys(&["q"]);
        for _ in 0..30 {
            if !session.is_alive() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert!(!session.is_alive(), "tmux session still alive after q");

        let _ = server.kill();
        let _ = server.wait();
    }
}
