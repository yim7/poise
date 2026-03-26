mod api_client;
mod app;
mod input;
mod protocol;
mod theme;
mod views;

use std::env;
use std::io::{self, Stdout};
use std::time::Duration;

use crate::api_client::{ApiClient, connect_ws};
use crate::app::{App, View};
use crate::input::{Action, CommandKind, handle_key_event};
use crate::protocol::{CommandResponse, WsEvent};
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

impl RuntimeConfig {
    fn from_env() -> Result<Self> {
        let base_url = env_value("GRID_TUI_BASE_URL")
            .or_else(|| env_value("GRID_PLATFORM_BASE_URL"))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let ws_url =
            match env_value("GRID_TUI_WS_URL").or_else(|| env_value("GRID_PLATFORM_WS_URL")) {
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
    let grids = client.list_instances().await?;
    let ids = grids
        .iter()
        .map(|instance| instance.id.clone())
        .collect::<Vec<_>>();
    let mut app = App::new(grids);

    for id in ids {
        let snapshot = client.get_snapshot(&id).await?;
        app.apply_snapshot(snapshot);
    }

    Ok(app)
}

async fn bootstrap_runtime_state(
    client: &ApiClient,
    ws_url: &str,
) -> (App, Option<tokio::sync::mpsc::Receiver<WsEvent>>) {
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
            app.mark_websocket_connected();
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
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<WsEvent>>,
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
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<WsEvent>>,
) {
    if ws_receiver.is_none() {
        maybe_reconnect_websocket(ws_url, app, ws_receiver).await;
        return;
    }

    let receiver = ws_receiver.as_mut().unwrap();

    match receiver.try_recv() {
        Ok(event) => {
            if let Err(error) = handle_ws_event(client, app, event).await {
                app.set_status_message(format!("ws refresh failed: {error}"));
            }
        }
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
            reconnect_websocket(ws_url, app, ws_receiver).await;
        }
    }
}

async fn maybe_reconnect_websocket(
    ws_url: &str,
    app: &mut App,
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<WsEvent>>,
) {
    if !app.should_retry_websocket() {
        return;
    }

    connect_websocket(ws_url, app, ws_receiver, "websocket connected").await;
}

async fn handle_action(client: &ApiClient, app: &mut App, action: Action) -> Result<()> {
    match action {
        Action::None => Ok(()),
        Action::OpenSelectedInstance | Action::RefreshSelectedInstance => {
            refresh_selected_snapshot(client, app).await?;
            app.show_instance_for_selected();
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
    let grid_id = app
        .selected_instance_id()
        .context("no instance selected for command")?
        .to_string();
    let response = client
        .submit_command(&grid_id, command.as_str())
        .await
        .with_context(|| format!("failed to submit command for `{grid_id}`"))?;
    if !response.accepted {
        anyhow::bail!(
            "command `{}` rejected for `{}`",
            response.command,
            response.grid_id
        );
    }
    app.set_status_message(format_command_response(&response));
    refresh_selected_snapshot(client, app).await?;
    if app.current_view == View::Instance {
        app.show_instance_for_selected();
    }
    Ok(())
}

fn format_command_response(response: &CommandResponse) -> String {
    format!(
        "command `{}` accepted for `{}`",
        response.command, response.grid_id
    )
}

async fn refresh_selected_snapshot(client: &ApiClient, app: &mut App) -> Result<()> {
    let grid_id = app
        .selected_instance_id()
        .context("no instance selected")?
        .to_string();
    let snapshot = client.get_snapshot(&grid_id).await?;
    app.apply_snapshot(snapshot);
    Ok(())
}

async fn handle_ws_event(client: &ApiClient, app: &mut App, event: WsEvent) -> Result<()> {
    let grid_id = event.grid_id.clone();
    app.record_event(event);
    let snapshot = client
        .get_snapshot(&grid_id)
        .await
        .with_context(|| format!("failed to refresh snapshot after ws event for `{grid_id}`"))?;
    app.apply_snapshot(snapshot);
    Ok(())
}

async fn reconnect_websocket(
    ws_url: &str,
    app: &mut App,
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<WsEvent>>,
) {
    app.set_status_message("websocket disconnected, reconnecting");
    connect_websocket(ws_url, app, ws_receiver, "websocket reconnected").await;
}

async fn connect_websocket(
    ws_url: &str,
    app: &mut App,
    ws_receiver: &mut Option<tokio::sync::mpsc::Receiver<WsEvent>>,
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
        app.mark_websocket_connected();
        app.set_status_message(success_message);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
    use axum::extract::{Path, Query, State};
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio::time::{Duration, sleep};

    use super::{
        ApiClient, CommandKind, View, bootstrap_runtime_state, derive_ws_url,
        format_command_response, handle_action, handle_ws_event, load_initial_state,
        maybe_load_initial_state, process_ws_event, submit_selected_command,
    };
    use crate::api_client::connect_ws;
    use crate::app::App;
    use crate::protocol::{
        CommandRequest, CommandResponse, DomainEvent, GridConfig, GridSnapshot, GridStatus,
        GridSummary, OutOfBandPolicy, ShapeFamily, WsEvent,
    };

    const BTC_GRID_ID: &str = "btc-core";
    const BTC_SYMBOL: &str = "BTCUSDT";
    const ETH_GRID_ID: &str = "eth-core";
    const ETH_SYMBOL: &str = "ETHUSDT";

    #[derive(Clone)]
    struct StubState {
        snapshots: Arc<Mutex<HashMap<String, GridSnapshot>>>,
    }

    fn btc_snapshot(exposure: f64, status: GridStatus) -> GridSnapshot {
        GridSnapshot {
            id: BTC_GRID_ID.into(),
            symbol: BTC_SYMBOL.into(),
            status,
            current_exposure: exposure,
            target_exposure: None,
            reference_price: Some(100.0),
            pending_order: None,
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        }
    }

    fn eth_snapshot() -> GridSnapshot {
        GridSnapshot {
            id: ETH_GRID_ID.into(),
            symbol: ETH_SYMBOL.into(),
            status: GridStatus::Paused,
            current_exposure: -1.0,
            target_exposure: None,
            reference_price: Some(2200.0),
            pending_order: None,
            config: GridConfig {
                lower_price: 2000.0,
                upper_price: 2600.0,
                long_exposure_units: 5.0,
                short_exposure_units: 4.0,
                notional_per_unit: 2000.0,
                shape_family: ShapeFamily::Concave,
                out_of_band_policy: OutOfBandPolicy::Hold,
            },
        }
    }

    async fn list_instances(State(state): State<StubState>) -> Json<Vec<GridSummary>> {
        let snapshots = state.snapshots.lock().await;
        Json(
            snapshots
                .values()
                .map(|snapshot| GridSummary {
                    id: snapshot.id.clone(),
                    symbol: snapshot.symbol.clone(),
                    status: snapshot.status.clone(),
                    reference_price: snapshot.reference_price,
                })
                .collect(),
        )
    }

    async fn get_snapshot(
        Path(id): Path<String>,
        State(state): State<StubState>,
    ) -> Json<GridSnapshot> {
        Json(state.snapshots.lock().await.get(&id).unwrap().clone())
    }

    async fn submit_command(
        Path(id): Path<String>,
        State(state): State<StubState>,
        Json(command): Json<CommandRequest>,
    ) -> Json<CommandResponse> {
        let mut snapshots = state.snapshots.lock().await;
        let snapshot = snapshots.get_mut(&id).unwrap();
        snapshot.status = match command.command.as_str() {
            "pause" => GridStatus::Paused,
            "resume" => GridStatus::Active,
            _ => snapshot.status.clone(),
        };

        Json(CommandResponse {
            grid_id: id,
            command: command.command,
            accepted: true,
        })
    }

    async fn ws_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(handle_ws_socket)
    }

    async fn handle_ws_socket(mut socket: WebSocket) {
        let payload = serde_json::to_string(&WsEvent {
            grid_id: BTC_GRID_ID.into(),
            event: DomainEvent::BandReentered { price: 101.0 },
        })
        .unwrap();
        socket.send(AxumMessage::Text(payload)).await.unwrap();
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
        let state = StubState {
            snapshots: Arc::new(Mutex::new(HashMap::from([
                (
                    BTC_GRID_ID.to_string(),
                    btc_snapshot(2.0, GridStatus::Active),
                ),
                (ETH_GRID_ID.to_string(), eth_snapshot()),
            ]))),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/grids", get(list_instances))
            .route("/grids/:id/snapshot", get(get_snapshot))
            .route("/grids/:id/commands", post(submit_command))
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
        let state = StubState {
            snapshots: Arc::new(Mutex::new(HashMap::from([
                (
                    BTC_GRID_ID.to_string(),
                    btc_snapshot(2.0, GridStatus::Active),
                ),
                (ETH_GRID_ID.to_string(), eth_snapshot()),
            ]))),
        };
        let listener = TcpListener::bind(address).await.unwrap();
        let app = Router::new()
            .route("/grids", get(list_instances))
            .route("/grids/:id/snapshot", get(get_snapshot))
            .route("/grids/:id/commands", post(submit_command))
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
        let text = format_command_response(&CommandResponse {
            grid_id: BTC_GRID_ID.into(),
            command: "pause".into(),
            accepted: true,
        });

        assert_eq!(text, "command `pause` accepted for `btc-core`");
    }

    #[tokio::test]
    async fn loads_initial_state_with_snapshots() {
        let (client, _, _) = spawn_stub_server().await;

        let app = load_initial_state(&client).await.unwrap();

        assert_eq!(app.instances.len(), 2);
        assert_eq!(
            app.cached_snapshot(BTC_GRID_ID).unwrap().current_exposure,
            2.0
        );
        assert_eq!(
            app.cached_snapshot(ETH_GRID_ID).unwrap().status,
            GridStatus::Paused
        );
    }

    #[tokio::test]
    async fn websocket_event_refreshes_cached_snapshot() {
        let (client, _, state) = spawn_stub_server().await;
        let mut app = load_initial_state(&client).await.unwrap();
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        state
            .snapshots
            .lock()
            .await
            .insert(BTC_GRID_ID.into(), btc_snapshot(4.0, GridStatus::Frozen));

        handle_ws_event(
            &client,
            &mut app,
            WsEvent {
                grid_id: BTC_GRID_ID.into(),
                event: DomainEvent::BandBreached {
                    boundary: crate::protocol::BandBoundary::Above,
                    price: 120.0,
                },
            },
        )
        .await
        .unwrap();

        assert_eq!(
            app.cached_snapshot(BTC_GRID_ID).unwrap().current_exposure,
            4.0
        );
        assert_eq!(
            app.current_instance.as_ref().unwrap().status,
            GridStatus::Frozen
        );
        assert_eq!(app.recent_events_for_current().len(), 1);
    }

    #[tokio::test]
    async fn submits_pause_command_and_refreshes_snapshot() {
        let (client, _, _) = spawn_stub_server().await;
        let mut app = load_initial_state(&client).await.unwrap();
        app.current_view = View::Instance;
        app.show_instance_for_selected();

        submit_selected_command(&client, &mut app, CommandKind::Pause)
            .await
            .unwrap();

        assert_eq!(
            app.cached_snapshot(BTC_GRID_ID).unwrap().status,
            GridStatus::Paused
        );
        assert!(
            app.status_message()
                .unwrap()
                .contains("command `pause` accepted")
        );
    }

    #[tokio::test]
    async fn startup_failure_still_returns_a_tui_state() {
        let client = ApiClient::new("http://127.0.0.1:1");

        let (app, ws_receiver) = bootstrap_runtime_state(&client, "ws://127.0.0.1:1/ws").await;

        assert!(app.instances.is_empty());
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
        assert!(app.instances.is_empty());
        assert!(app.status_message().unwrap().contains("startup failed"));

        let (_, _, _, server) = spawn_stub_server_on(bind_address).await;

        for _ in 0..20 {
            maybe_load_initial_state(&client, &mut app).await;
            process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;
            if app.instances.len() == 2 && app.cached_snapshot(BTC_GRID_ID).is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert_eq!(app.instances.len(), 2);
        assert!(app.cached_snapshot(BTC_GRID_ID).is_some());
        assert!(app.cached_snapshot(ETH_GRID_ID).is_some());

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
            if !app.recent_events_for_current().is_empty() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert!(ws_receiver.is_some());
        assert_eq!(app.recent_events_for_current().len(), 1);

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
        assert_eq!(event.grid_id, BTC_GRID_ID);
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
            if !app.recent_events_for_current().is_empty() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert!(ws_receiver.is_some());
        assert_eq!(app.recent_events_for_current().len(), 1);

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

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn grid_tui_binary_path() -> PathBuf {
        let mut path = workspace_root().join("target").join("debug");
        path.push(if cfg!(windows) {
            "grid-tui.exe"
        } else {
            "grid-tui"
        });
        path
    }

    fn grid_server_binary_path() -> PathBuf {
        let mut path = workspace_root().join("target").join("debug");
        path.push(if cfg!(windows) {
            "grid-server.exe"
        } else {
            "grid-server"
        });
        path
    }

    fn ensure_grid_server_binary() -> PathBuf {
        let path = grid_server_binary_path();
        let status = Command::new("cargo")
            .arg("build")
            .arg("-p")
            .arg("grid-server")
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
            .arg("grid-tui")
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
                "grid-tui-{}",
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

    async fn wait_for_http_ready(base_url: &str) {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .unwrap();
        for _ in 0..50 {
            let Ok(response) = client.get(format!("{base_url}/grids")).send().await else {
                sleep(Duration::from_millis(100)).await;
                continue;
            };
            if response.status().is_success() {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }

        panic!("server did not become ready");
    }

    async fn wait_for_snapshot_price(client: &ApiClient, id: &str) {
        for _ in 0..50 {
            let snapshot = client.get_snapshot(id).await.unwrap();
            if snapshot.reference_price.is_some() {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }

        panic!("snapshot `{id}` never received market price");
    }

    #[tokio::test]
    async fn real_server_protocol_integration_covers_list_switch_and_ws_updates() {
        let exchange = spawn_fake_exchange_server().await;
        let server_binary = ensure_grid_server_binary();
        let temp_dir = tempfile::tempdir().unwrap();
        let bind_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind_address = bind_listener.local_addr().unwrap();
        drop(bind_listener);
        let config_path = temp_dir.path().join("grid-server.toml");
        fs::write(
            &config_path,
            format!(
                r#"
environment = "tui-e2e"
bind_address = "{bind_address}"

[exchange]
rest_base_url = "{rest_base_url}"
ws_base_url = "{ws_base_url}"

[[grids]]
grid_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0

[[grids]]
grid_id = "eth-core"
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
                rest_base_url = exchange.rest_base_url,
                ws_base_url = exchange.ws_base_url
            ),
        )
        .unwrap();

        let mut server = Command::new(server_binary)
            .arg("--config")
            .arg(&config_path)
            .current_dir(temp_dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let base_url = format!("http://{bind_address}");
        let ws_url = format!("ws://{bind_address}/ws");
        wait_for_http_ready(&base_url).await;
        let mut ws_receiver = Some(connect_ws(&ws_url).await.unwrap());

        let client = ApiClient::new(base_url);
        wait_for_snapshot_price(&client, BTC_GRID_ID).await;
        wait_for_snapshot_price(&client, ETH_GRID_ID).await;

        let mut app = load_initial_state(&client).await.unwrap();
        assert_eq!(app.instances.len(), 2);
        assert!(
            app.cached_snapshot(BTC_GRID_ID)
                .unwrap()
                .reference_price
                .is_some()
        );
        assert!(
            app.cached_snapshot(ETH_GRID_ID)
                .unwrap()
                .reference_price
                .is_some()
        );

        let action = crate::input::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        handle_action(&client, &mut app, action).await.unwrap();
        assert_eq!(app.current_instance.as_ref().unwrap().id, BTC_GRID_ID);

        let action = crate::input::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
        );
        handle_action(&client, &mut app, action).await.unwrap();
        assert_eq!(app.current_instance.as_ref().unwrap().id, ETH_GRID_ID);

        for _ in 0..30 {
            process_ws_event(&client, &ws_url, &mut app, &mut ws_receiver).await;
            if !app.recent_events_for_current().is_empty() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        assert!(!app.recent_events_for_current().is_empty());

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
        let config_path = temp_dir.path().join("grid-server.toml");
        fs::write(
            &config_path,
            format!(
                r#"
environment = "tui-binary-e2e"
bind_address = "{bind_address}"

[exchange]
rest_base_url = "{rest_base_url}"
ws_base_url = "{ws_base_url}"

[[grids]]
grid_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0

[[grids]]
grid_id = "eth-core"
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
                rest_base_url = exchange.rest_base_url,
                ws_base_url = exchange.ws_base_url
            ),
        )
        .unwrap();

        let mut server = Command::new(server_binary)
            .arg("--config")
            .arg(&config_path)
            .current_dir(temp_dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let base_url = format!("http://{bind_address}");
        let ws_url = format!("ws://{bind_address}/ws");
        wait_for_http_ready(&base_url).await;
        let session = TmuxSession::start(&format!(
            "env GRID_TUI_BASE_URL={base_url} GRID_TUI_WS_URL={ws_url} {}",
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
            eth_view.contains("capacity notional: 2000.0000"),
            "eth view:\n{eth_view}"
        );

        let event_view = wait_for_pane_text(&session, "snapshot updated").await;
        assert!(
            event_view.contains("Recent Events"),
            "event view:\n{event_view}"
        );
        assert!(
            event_view.contains("snapshot updated"),
            "event view:\n{event_view}"
        );

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
