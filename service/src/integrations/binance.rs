use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{SecondsFormat, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use reqwest::{Method, header::CONTENT_TYPE};
use serde::Deserialize;
use sha2::Sha256;
use tokio::{
    select,
    sync::{Mutex, mpsc},
    time::{Instant, interval, sleep},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::warn;

use crate::{
    background::spawn_task,
    execution::{
        CancelOrdersRequest, ExecutionAdapter, FakeExecutionAdapter, SubmitOrderRequest,
        SubmitOrderResult,
    },
    kernel::{EngineHandle, RuntimePatch},
    protocol::{
        ConnectionState, ExchangeOrderRules, OpenOrder, OpenOrdersSource, RecentFill,
        RuntimeSnapshot,
    },
    storage::PersistedRuntime,
};

const MARKET_STREAM_BUFFER: usize = 256;
const USER_STREAM_BUFFER: usize = 128;
const SIGNED_RECV_WINDOW_MS: i64 = 5_000;

#[derive(Debug, Clone)]
pub struct BinanceConfig {
    pub symbol: String,
    pub env: String,
    pub rest_base_url: String,
    pub ws_base_url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub metadata_refresh_interval: Duration,
    pub health_tick_interval: Duration,
    pub reconnect_base_delay: Duration,
    pub reconnect_max_delay: Duration,
    pub user_stream_keepalive_interval: Duration,
}

#[derive(Debug, Clone)]
enum MarketHeartbeatState {
    Disconnected,
    WaitingForFirstEvent { connected_at: Instant },
    Live { last_event_at: Instant },
}

impl BinanceConfig {
    pub fn mainnet(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            env: "mainnet".into(),
            rest_base_url: "https://fapi.binance.com".into(),
            ws_base_url: "wss://fstream.binance.com".into(),
            api_key: None,
            api_secret: None,
            metadata_refresh_interval: Duration::from_secs(300),
            health_tick_interval: Duration::from_secs(1),
            reconnect_base_delay: Duration::from_secs(1),
            reconnect_max_delay: Duration::from_secs(30),
            user_stream_keepalive_interval: Duration::from_secs(30 * 60),
        }
    }

    pub fn testnet(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            env: "testnet".into(),
            rest_base_url: "https://demo-fapi.binance.com".into(),
            ws_base_url: "wss://fstream.binancefuture.com".into(),
            api_key: None,
            api_secret: None,
            metadata_refresh_interval: Duration::from_secs(300),
            health_tick_interval: Duration::from_secs(1),
            reconnect_base_delay: Duration::from_secs(1),
            reconnect_max_delay: Duration::from_secs(30),
            user_stream_keepalive_interval: Duration::from_secs(30 * 60),
        }
    }
}

pub fn prepare_bootstrap_runtime(
    mut runtime: PersistedRuntime,
    config: &BinanceConfig,
) -> PersistedRuntime {
    let symbol_or_env_changed = runtime.snapshot.runtime.symbol != config.symbol
        || runtime.snapshot.runtime.env != config.env;
    let should_reset_runtime = symbol_or_env_changed || runtime.last_sequence == 0;

    runtime.snapshot.runtime.symbol = config.symbol.clone();
    runtime.snapshot.runtime.env = config.env.clone();
    runtime.snapshot.connection.http_available = false;
    runtime.snapshot.connection.ws_connected = false;
    runtime.snapshot.connection.user_stream_connected = config.api_key.as_ref().map(|_| false);
    runtime.snapshot.connection.last_heartbeat_at.clear();
    runtime.snapshot.connection.reconnect_backoff_ms = 0;
    runtime.snapshot.connection.stale_age_ms = 0;
    runtime.snapshot.execution.exchange_open_orders.clear();
    runtime.snapshot.execution.exchange_open_orders_source = OpenOrdersSource::Unavailable;

    if should_reset_runtime {
        runtime.snapshot.runtime.session_state = "syncing".into();
        runtime.snapshot.runtime.last_price = 0.0;
        runtime.snapshot.runtime.mark_price = 0.0;
        runtime.snapshot.runtime.position_qty = 0.0;
        runtime.snapshot.runtime.position_avg_price = 0.0;
        runtime.snapshot.runtime.unrealized_pnl = 0.0;
        runtime.snapshot.runtime.realized_pnl = 0.0;
        runtime.snapshot.execution.open_orders.clear();
        runtime.snapshot.execution.recent_fills.clear();
        runtime.snapshot.execution.pending_commands.clear();
        runtime.snapshot.execution.last_command_ack = None;
        runtime.snapshot.execution.last_command_ack_event = None;
        runtime.snapshot.execution.recent_commands.clear();
        runtime.snapshot.execution.exchange_open_orders.clear();
    }

    runtime.snapshot.execution.open_orders_source = OpenOrdersSource::StrategyMirror;

    runtime
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExchangeSymbol {
    pub symbol: String,
    pub status: String,
    pub underlying_type: String,
    pub order_rules: Option<ExchangeOrderRules>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TradingSchedule {
    pub update_time_ms: i64,
    pub market_schedules: HashMap<String, Vec<TradingSession>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TradingSession {
    pub start_time_ms: i64,
    pub end_time_ms: i64,
    pub session_type: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarketStreamEvent {
    pub event_time_ms: i64,
    pub last_price: Option<f64>,
    pub mark_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserStreamEvent {
    pub event_time_ms: i64,
    pub positions: Vec<PositionSnapshot>,
    pub order_updates: Vec<UserStreamOrderUpdate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PositionSnapshot {
    pub symbol: String,
    pub qty: f64,
    pub avg_price: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionMode {
    Unavailable,
    OneWay,
    Hedge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PositionSnapshotState {
    Unavailable,
    Flat,
    Position(PositionSnapshot),
}

impl PositionSnapshotState {
    pub fn as_ref(&self) -> Option<&PositionSnapshot> {
        match self {
            Self::Position(snapshot) => Some(snapshot),
            Self::Unavailable | Self::Flat => None,
        }
    }

    pub fn is_available(&self) -> bool {
        !matches!(self, Self::Unavailable)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserStreamOrderUpdate {
    pub symbol: String,
    pub order: OpenOrder,
    pub is_terminal: bool,
}

#[async_trait]
pub trait BinanceTransport: Send + Sync + 'static {
    async fn fetch_exchange_info(&self, symbol: &str) -> Result<ExchangeSymbol>;
    async fn fetch_trading_schedule(&self) -> Result<TradingSchedule>;
    fn supports_execution(&self) -> bool {
        false
    }
    async fn fetch_position_mode(&self) -> Result<PositionMode> {
        Ok(PositionMode::Unavailable)
    }
    async fn fetch_position_snapshot(&self, _symbol: &str) -> Result<PositionSnapshotState> {
        Ok(PositionSnapshotState::Unavailable)
    }
    async fn connect_market_stream(
        &self,
        symbol: &str,
    ) -> Result<mpsc::Receiver<MarketStreamEvent>>;
    async fn create_user_stream(&self) -> Result<Option<String>>;
    async fn connect_user_stream(
        &self,
        listen_key: &str,
    ) -> Result<mpsc::Receiver<UserStreamEvent>>;
    async fn keepalive_user_stream(&self, listen_key: &str) -> Result<()>;
    async fn fetch_open_orders(&self, _symbol: &str) -> Result<Option<Vec<OpenOrder>>> {
        Ok(None)
    }
    async fn submit_order(
        &self,
        _request: SubmitOrderRequest,
        _snapshot: &RuntimeSnapshot,
    ) -> Result<SubmitOrderResult> {
        Err(anyhow!("binance transport execution is not available"))
    }
    async fn cancel_orders(
        &self,
        _request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<Vec<OpenOrder>> {
        Ok(snapshot.execution.open_orders.clone())
    }
}

pub(crate) fn execution_adapter_for_binance(
    config: &BinanceConfig,
    transport: Arc<dyn BinanceTransport>,
) -> Arc<dyn ExecutionAdapter> {
    if config.api_key.is_some() && config.api_secret.is_some() && transport.supports_execution() {
        Arc::new(BinanceExecutionAdapter { transport })
    } else {
        Arc::new(FakeExecutionAdapter)
    }
}

#[derive(Clone)]
struct BinanceExecutionAdapter {
    transport: Arc<dyn BinanceTransport>,
}

#[async_trait]
impl ExecutionAdapter for BinanceExecutionAdapter {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<SubmitOrderResult> {
        self.transport.submit_order(request, snapshot).await
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<Vec<OpenOrder>> {
        let open_orders = self.transport.cancel_orders(request, snapshot).await?;
        Ok(filter_strategy_mirror_open_orders(snapshot, open_orders))
    }

    async fn query_open_orders(&self, snapshot: &RuntimeSnapshot) -> Result<Vec<OpenOrder>> {
        Ok(filter_strategy_mirror_open_orders(
            snapshot,
            self.transport
                .fetch_open_orders(&snapshot.runtime.symbol)
                .await?
                .unwrap_or_default(),
        ))
    }

    async fn list_recent_fills(&self, snapshot: &RuntimeSnapshot) -> Result<Vec<RecentFill>> {
        Ok(snapshot.execution.recent_fills.clone())
    }
}

#[derive(Debug, Clone)]
pub struct RealBinanceTransport {
    http: reqwest::Client,
    rest_base_url: String,
    ws_base_url: String,
    api_key: Option<String>,
    api_secret: Option<String>,
    signed_time_offset_ms: Arc<Mutex<Option<i64>>>,
}

impl RealBinanceTransport {
    pub fn new(config: &BinanceConfig) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("grid-platform-service/0.1.0")
                .build()
                .context("failed to build reqwest client")?,
            rest_base_url: config.rest_base_url.clone(),
            ws_base_url: config.ws_base_url.clone(),
            api_key: config.api_key.clone(),
            api_secret: config.api_secret.clone(),
            signed_time_offset_ms: Arc::new(Mutex::new(None)),
        })
    }

    async fn get_json<T>(&self, path: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = format!("{}{}", self.rest_base_url, path);
        let mut request = self.http.get(&url);
        if let Some(api_key) = &self.api_key {
            request = request.header("X-MBX-APIKEY", api_key);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?;
        decode_response(response).await
    }

    async fn get_signed_json<T>(&self, path: &str, query: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let (status, body) = self.signed_response_text(Method::GET, path, query).await?;
        decode_response_body(status, body)
    }

    async fn signed_response_text(
        &self,
        method: Method,
        path: &str,
        query: &str,
    ) -> Result<(reqwest::StatusCode, String)> {
        let response = self
            .send_signed_request(method.clone(), path, query, false)
            .await?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read response body")?;
        if is_timestamp_outside_recv_window(status, &body) {
            let retry = self.send_signed_request(method, path, query, true).await?;
            let retry_status = retry.status();
            let retry_body = retry.text().await.context("failed to read response body")?;
            return Ok((retry_status, retry_body));
        }
        Ok((status, body))
    }

    async fn send_signed_request(
        &self,
        method: Method,
        path: &str,
        query: &str,
        force_time_sync: bool,
    ) -> Result<reqwest::Response> {
        let api_key = self
            .api_key
            .as_ref()
            .context("binance api_key is required for signed requests")?;
        let api_secret = self
            .api_secret
            .as_ref()
            .context("binance api_secret is required for signed requests")?;
        let signed_query = self.signed_query(query, force_time_sync).await?;
        let signature = sign_query(&signed_query, api_secret)?;
        let url = format!(
            "{}{}?{}&signature={}",
            self.rest_base_url, path, signed_query, signature
        );
        self.http
            .request(method, &url)
            .header("X-MBX-APIKEY", api_key)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))
    }

    async fn signed_query(&self, query: &str, force_time_sync: bool) -> Result<String> {
        let timestamp_ms = self.signed_timestamp_millis(force_time_sync).await?;
        if query.is_empty() {
            return Ok(format!(
                "recvWindow={SIGNED_RECV_WINDOW_MS}&timestamp={timestamp_ms}"
            ));
        }
        Ok(format!(
            "{query}&recvWindow={SIGNED_RECV_WINDOW_MS}&timestamp={timestamp_ms}"
        ))
    }

    async fn signed_timestamp_millis(&self, force_time_sync: bool) -> Result<i64> {
        let offset_ms = self.load_signed_time_offset(force_time_sync).await?;
        Ok(Utc::now().timestamp_millis() + offset_ms)
    }

    async fn load_signed_time_offset(&self, force_time_sync: bool) -> Result<i64> {
        if !force_time_sync && let Some(offset_ms) = *self.signed_time_offset_ms.lock().await {
            return Ok(offset_ms);
        }

        let response: ServerTimeResponse = self.get_json("/fapi/v1/time").await?;
        let offset_ms = response.server_time_ms - Utc::now().timestamp_millis();
        *self.signed_time_offset_ms.lock().await = Some(offset_ms);
        Ok(offset_ms)
    }

    async fn create_listen_key(&self) -> Result<Option<String>> {
        let Some(api_key) = &self.api_key else {
            return Ok(None);
        };

        let url = format!("{}/fapi/v1/listenKey", self.rest_base_url);
        let response = self
            .http
            .post(&url)
            .header("X-MBX-APIKEY", api_key)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("binance listenKey request failed: {body}");
        }

        let parsed: ListenKeyResponse = response
            .json()
            .await
            .context("failed to decode listenKey response")?;
        Ok(Some(parsed.listen_key))
    }

    async fn keepalive_listen_key(&self) -> Result<()> {
        let Some(api_key) = &self.api_key else {
            return Ok(());
        };

        let url = format!("{}/fapi/v1/listenKey", self.rest_base_url);
        let response = self
            .http
            .put(&url)
            .header("X-MBX-APIKEY", api_key)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .send()
            .await
            .with_context(|| format!("failed to call {url}"))?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("binance listenKey keepalive failed: {body}");
        }
        Ok(())
    }
}

fn filter_strategy_mirror_open_orders(
    snapshot: &RuntimeSnapshot,
    open_orders: Vec<OpenOrder>,
) -> Vec<OpenOrder> {
    open_orders
        .into_iter()
        .filter(|candidate| {
            snapshot.execution.open_orders.iter().any(|known| {
                known.order_id == candidate.order_id
                    || known.client_order_id == candidate.client_order_id
            })
        })
        .collect()
}

#[async_trait]
impl BinanceTransport for RealBinanceTransport {
    async fn fetch_exchange_info(&self, symbol: &str) -> Result<ExchangeSymbol> {
        let response: ExchangeInfoResponse = self.get_json("/fapi/v1/exchangeInfo").await?;
        let symbol_info = response
            .symbols
            .into_iter()
            .find(|item| item.symbol == symbol)
            .ok_or_else(|| anyhow!("symbol {symbol} not found in exchangeInfo"))?;
        let order_rules = symbol_info.into_order_rules();
        Ok(ExchangeSymbol {
            symbol: symbol_info.symbol,
            status: symbol_info.status,
            underlying_type: symbol_info.underlying_type,
            order_rules,
        })
    }

    async fn fetch_trading_schedule(&self) -> Result<TradingSchedule> {
        let response: TradingScheduleResponse = self.get_json("/fapi/v1/tradingSchedule").await?;
        Ok(TradingSchedule {
            update_time_ms: response.update_time,
            market_schedules: response
                .market_schedules
                .into_iter()
                .map(|(market, schedule)| {
                    (
                        market,
                        schedule
                            .sessions
                            .into_iter()
                            .map(|session| TradingSession {
                                start_time_ms: session.start_time,
                                end_time_ms: session.end_time,
                                session_type: session.session_type,
                            })
                            .collect(),
                    )
                })
                .collect(),
        })
    }

    fn supports_execution(&self) -> bool {
        true
    }

    async fn fetch_position_mode(&self) -> Result<PositionMode> {
        if self.api_key.is_none() || self.api_secret.is_none() {
            return Ok(PositionMode::Unavailable);
        }

        let response: PositionModeResponse = self
            .get_signed_json("/fapi/v1/positionSide/dual", "")
            .await?;
        Ok(if response.dual_side_position {
            PositionMode::Hedge
        } else {
            PositionMode::OneWay
        })
    }

    async fn connect_market_stream(
        &self,
        symbol: &str,
    ) -> Result<mpsc::Receiver<MarketStreamEvent>> {
        let stream_symbol = symbol.to_ascii_lowercase();
        let url = format!(
            "{}/stream?streams={}@aggTrade/{}@markPrice@1s",
            self.ws_base_url, stream_symbol, stream_symbol
        );
        let (socket, _) = connect_async(&url)
            .await
            .with_context(|| format!("failed to connect market stream {url}"))?;
        let (tx, rx) = mpsc::channel(MARKET_STREAM_BUFFER);
        spawn_task(async move {
            let mut socket = socket;
            while let Some(message) = socket.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        if let Ok(event) = decode_market_stream(&text)
                            && tx.send(event).await.is_err()
                        {
                            break;
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(error) => {
                        warn!(?error, "market websocket read failed");
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }

    async fn create_user_stream(&self) -> Result<Option<String>> {
        self.create_listen_key().await
    }

    async fn connect_user_stream(
        &self,
        listen_key: &str,
    ) -> Result<mpsc::Receiver<UserStreamEvent>> {
        let url = format!("{}/ws/{}", self.ws_base_url, listen_key);
        let (socket, _) = connect_async(&url)
            .await
            .with_context(|| format!("failed to connect user stream {url}"))?;
        let (tx, rx) = mpsc::channel(USER_STREAM_BUFFER);
        spawn_task(async move {
            let mut socket = socket;
            while let Some(message) = socket.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        if let Ok(Some(event)) = decode_user_stream(&text)
                            && tx.send(event).await.is_err()
                        {
                            break;
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(error) => {
                        warn!(?error, "user websocket read failed");
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }

    async fn keepalive_user_stream(&self, _listen_key: &str) -> Result<()> {
        self.keepalive_listen_key().await
    }

    async fn fetch_position_snapshot(&self, symbol: &str) -> Result<PositionSnapshotState> {
        if self.api_key.is_none() || self.api_secret.is_none() {
            return Ok(PositionSnapshotState::Unavailable);
        }

        let query = format!("symbol={symbol}");
        let positions: Vec<PositionRiskResponsePosition> = self
            .get_signed_json("/fapi/v3/positionRisk", &query)
            .await?;
        Ok(match summarize_position_risk(positions) {
            Some(position) => PositionSnapshotState::Position(position),
            None => PositionSnapshotState::Flat,
        })
    }

    async fn fetch_open_orders(&self, symbol: &str) -> Result<Option<Vec<OpenOrder>>> {
        if self.api_key.is_none() || self.api_secret.is_none() {
            return Ok(None);
        }

        let query = format!("symbol={symbol}");
        let orders: Vec<OpenOrdersResponseOrder> =
            self.get_signed_json("/fapi/v1/openOrders", &query).await?;
        Ok(Some(
            orders
                .into_iter()
                .map(OpenOrdersResponseOrder::into_open_order)
                .collect(),
        ))
    }

    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<SubmitOrderResult> {
        let symbol = &snapshot.runtime.symbol;
        let query = build_submit_order_query(symbol, &request);
        let (status, body) = self
            .signed_response_text(Method::POST, "/fapi/v1/order", &query)
            .await?;
        if is_position_mode_mismatch_error(status, &body) {
            anyhow::bail!("binance hedge mode is enabled; grid strategy requires one-way mode");
        }
        let response: SubmitOrderResponse = decode_response_body(status, body)?;
        Ok(response.into_submit_result(&request))
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<Vec<OpenOrder>> {
        let symbol = &snapshot.runtime.symbol;
        let targets = request
            .order_ids
            .iter()
            .map(Some)
            .chain(std::iter::repeat(None))
            .zip(
                request
                    .client_order_ids
                    .iter()
                    .map(Some)
                    .chain(std::iter::repeat(None)),
            )
            .take(request.order_ids.len().max(request.client_order_ids.len()));

        for (order_id, client_order_id) in targets {
            if order_id.is_none() && client_order_id.is_none() {
                continue;
            }

            let query = build_cancel_order_query(symbol, order_id, client_order_id);
            let (status, body) = self
                .signed_response_text(Method::DELETE, "/fapi/v1/order", &query)
                .await?;
            if status.is_success() || is_missing_order_error(status, &body) {
                continue;
            }
            anyhow::bail!("binance request failed with status {status}: {body}");
        }

        Ok(self.fetch_open_orders(symbol).await?.unwrap_or_default())
    }
}

pub(crate) fn spawn_supervisor(
    engine: EngineHandle,
    initial_connection: ConnectionState,
    config: BinanceConfig,
    transport: Arc<dyn BinanceTransport>,
) {
    spawn_task(async move {
        let reporter = ConnectionReporter::new(engine.clone(), initial_connection);
        let _ = reporter.publish().await;

        let last_market_heartbeat = Arc::new(Mutex::new(MarketHeartbeatState::Disconnected));

        spawn_task(run_metadata_loop(
            engine.clone(),
            reporter.clone(),
            config.clone(),
            transport.clone(),
        ));
        spawn_task(run_market_loop(
            engine.clone(),
            reporter.clone(),
            config.clone(),
            transport.clone(),
            last_market_heartbeat.clone(),
        ));
        spawn_task(run_health_loop(
            reporter.clone(),
            config.clone(),
            last_market_heartbeat,
        ));
        if config.api_key.is_some() {
            spawn_task(run_user_stream_loop(engine, reporter, config, transport));
        }
    });
}

#[derive(Clone)]
struct ConnectionReporter {
    engine: EngineHandle,
    state: Arc<Mutex<ConnectionState>>,
}

impl ConnectionReporter {
    fn new(engine: EngineHandle, state: ConnectionState) -> Self {
        Self {
            engine,
            state: Arc::new(Mutex::new(state)),
        }
    }

    async fn mutate_local<F>(&self, mutate: F)
    where
        F: FnOnce(&mut ConnectionState),
    {
        let mut guard = self.state.lock().await;
        mutate(&mut guard);
    }

    async fn update<F>(&self, mutate: F) -> Result<()>
    where
        F: FnOnce(&mut ConnectionState),
    {
        let mut guard = self.state.lock().await;
        let before = guard.clone();
        mutate(&mut guard);
        if *guard == before {
            return Ok(());
        }
        let next = guard.clone();
        match self.engine.sync_connection(next).await {
            Ok(()) => Ok(()),
            Err(error) => {
                *guard = before;
                Err(error)
            }
        }
    }

    async fn update_until_applied<F>(&self, mutate: F, retry_delay: Duration, context: &'static str)
    where
        F: Fn(&mut ConnectionState) + Copy,
    {
        loop {
            match self.update(mutate).await {
                Ok(()) => return,
                Err(error) => {
                    warn!(?error, %context, "failed to persist connection state; retrying");
                    sleep(retry_delay).await;
                }
            }
        }
    }

    async fn publish(&self) -> Result<()> {
        let snapshot = self.state.lock().await.clone();
        self.engine.sync_connection(snapshot).await
    }
}

async fn run_metadata_loop(
    engine: EngineHandle,
    reporter: ConnectionReporter,
    config: BinanceConfig,
    transport: Arc<dyn BinanceTransport>,
) {
    let mut ticker = interval(config.metadata_refresh_interval);
    loop {
        let result = async {
            let exchange_info = transport.fetch_exchange_info(&config.symbol).await?;
            let schedule = transport.fetch_trading_schedule().await?;
            let session_state =
                derive_session_state(&exchange_info, &schedule, Utc::now().timestamp_millis());
            engine
                .sync_runtime(RuntimePatch {
                    session_state: Some(session_state),
                    exchange_rules: Some(exchange_info.order_rules.clone()),
                    ..RuntimePatch::default()
                })
                .await?;
            reporter
                .update(|state| {
                    state.http_available = true;
                })
                .await
        }
        .await;

        if let Err(error) = result {
            warn!(?error, "binance metadata sync failed");
            let _ = reporter
                .update(|state| {
                    state.http_available = false;
                })
                .await;
        }

        ticker.tick().await;
    }
}

async fn run_market_loop(
    engine: EngineHandle,
    reporter: ConnectionReporter,
    config: BinanceConfig,
    transport: Arc<dyn BinanceTransport>,
    last_market_heartbeat: Arc<Mutex<MarketHeartbeatState>>,
) {
    let mut attempt = 0u32;
    loop {
        match transport.connect_market_stream(&config.symbol).await {
            Ok(mut events) => {
                attempt = 0;
                {
                    let mut heartbeat = last_market_heartbeat.lock().await;
                    *heartbeat = MarketHeartbeatState::WaitingForFirstEvent {
                        connected_at: Instant::now(),
                    };
                }
                reporter
                    .update_until_applied(
                        |state| {
                            state.ws_connected = true;
                            state.reconnect_backoff_ms = 0;
                            state.last_heartbeat_at.clear();
                            state.stale_age_ms = 0;
                        },
                        config.reconnect_base_delay,
                        "binance market stream connected",
                    )
                    .await;

                while let Some(event) = events.recv().await {
                    *last_market_heartbeat.lock().await = MarketHeartbeatState::Live {
                        last_event_at: Instant::now(),
                    };
                    reporter
                        .mutate_local(|state| {
                            state.ws_connected = true;
                            state.reconnect_backoff_ms = 0;
                            state.last_heartbeat_at =
                                timestamp_millis_to_rfc3339(event.event_time_ms);
                            state.stale_age_ms = 0;
                        })
                        .await;
                    sync_market_prices_until_applied(&engine, event, config.reconnect_base_delay)
                        .await;
                }
            }
            Err(error) => {
                warn!(?error, "binance market stream connect failed");
            }
        }

        attempt = attempt.saturating_add(1);
        let backoff = reconnect_delay(&config, attempt);
        let backoff_ms = backoff.as_millis().min(u64::MAX as u128) as u64;
        {
            let mut heartbeat = last_market_heartbeat.lock().await;
            *heartbeat = MarketHeartbeatState::Disconnected;
        }
        reporter
            .update_until_applied(
                |state| {
                    state.ws_connected = false;
                    state.reconnect_backoff_ms = backoff_ms;
                    state.last_heartbeat_at.clear();
                    state.stale_age_ms = 0;
                },
                config.reconnect_base_delay,
                "binance market stream disconnected",
            )
            .await;
        sleep(backoff).await;
    }
}

async fn run_health_loop(
    reporter: ConnectionReporter,
    config: BinanceConfig,
    last_market_heartbeat: Arc<Mutex<MarketHeartbeatState>>,
) {
    let mut ticker = interval(config.health_tick_interval);
    loop {
        ticker.tick().await;
        let stale_age_ms = match &*last_market_heartbeat.lock().await {
            MarketHeartbeatState::Disconnected => 0,
            MarketHeartbeatState::WaitingForFirstEvent { connected_at } => {
                connected_at.elapsed().as_millis().min(u64::MAX as u128) as u64
            }
            MarketHeartbeatState::Live { last_event_at } => {
                last_event_at.elapsed().as_millis().min(u64::MAX as u128) as u64
            }
        };
        let _ = reporter
            .update(|state| {
                state.stale_age_ms = stale_age_ms;
            })
            .await;
    }
}

async fn run_user_stream_loop(
    engine: EngineHandle,
    reporter: ConnectionReporter,
    config: BinanceConfig,
    transport: Arc<dyn BinanceTransport>,
) {
    let mut attempt = 0u32;
    loop {
        let listen_key = match transport.create_user_stream().await {
            Ok(Some(listen_key)) => listen_key,
            Ok(None) => {
                reporter
                    .update_until_applied(
                        |state| state.user_stream_connected = None,
                        config.reconnect_base_delay,
                        "binance user stream disabled",
                    )
                    .await;
                return;
            }
            Err(error) => {
                warn!(?error, "binance user stream listenKey request failed");
                attempt = attempt.saturating_add(1);
                reporter
                    .update_until_applied(
                        |state| state.user_stream_connected = Some(false),
                        config.reconnect_base_delay,
                        "binance user stream listenKey failed",
                    )
                    .await;
                sleep(reconnect_delay(&config, attempt)).await;
                continue;
            }
        };

        match transport.connect_user_stream(&listen_key).await {
            Ok(mut events) => {
                attempt = 0;
                reporter
                    .update_until_applied(
                        |state| state.user_stream_connected = Some(true),
                        config.reconnect_base_delay,
                        "binance user stream connected",
                    )
                    .await;
                let mut exchange_open_orders =
                    match transport.fetch_open_orders(&config.symbol).await {
                        Ok(Some(orders)) => {
                            sync_exchange_open_orders_until_applied(
                                &engine,
                                orders.clone(),
                                OpenOrdersSource::ExchangeLive,
                                config.reconnect_base_delay,
                                "binance open orders bootstrap",
                            )
                            .await;
                            orders
                        }
                        Ok(None) => {
                            sync_exchange_open_orders_until_applied(
                                &engine,
                                Vec::new(),
                                OpenOrdersSource::Unavailable,
                                config.reconnect_base_delay,
                                "binance open orders unavailable",
                            )
                            .await;
                            Vec::new()
                        }
                        Err(error) => {
                            warn!(?error, "binance open orders bootstrap failed");
                            sync_exchange_open_orders_until_applied(
                                &engine,
                                Vec::new(),
                                OpenOrdersSource::Unavailable,
                                config.reconnect_base_delay,
                                "binance open orders bootstrap failed",
                            )
                            .await;
                            Vec::new()
                        }
                    };
                let mut keepalive = interval(config.user_stream_keepalive_interval);
                loop {
                    select! {
                        maybe_event = events.recv() => {
                            let Some(event) = maybe_event else {
                                break;
                            };
                            if !event.order_updates.is_empty() && config.api_secret.is_some() {
                                for update in event.order_updates {
                                    if update.symbol != config.symbol {
                                        continue;
                                    }
                                    apply_order_update(&mut exchange_open_orders, update);
                                }
                                sync_exchange_open_orders_until_applied(
                                    &engine,
                                    exchange_open_orders.clone(),
                                    OpenOrdersSource::ExchangeLive,
                                    config.reconnect_base_delay,
                                    "binance user stream order sync",
                                )
                                .await;
                            }
                            if let Some(position) = event
                                .positions
                                .into_iter()
                                .find(|position| position.symbol == config.symbol)
                            {
                                sync_runtime_patch_until_applied(
                                    &engine,
                                    RuntimePatch {
                                        position_qty: Some(position.qty),
                                        position_avg_price: Some(position.avg_price),
                                        unrealized_pnl: Some(position.unrealized_pnl),
                                        realized_pnl: Some(position.realized_pnl),
                                        ..RuntimePatch::default()
                                    },
                                    config.reconnect_base_delay,
                                    "binance user stream position sync",
                                )
                                .await;
                            }
                        }
                        _ = keepalive.tick() => {
                            if let Err(error) = transport.keepalive_user_stream(&listen_key).await {
                                warn!(?error, "binance user stream keepalive failed");
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                warn!(?error, "binance user stream connect failed");
            }
        }

        attempt = attempt.saturating_add(1);
        reporter
            .update_until_applied(
                |state| state.user_stream_connected = Some(false),
                config.reconnect_base_delay,
                "binance user stream disconnected",
            )
            .await;
        sleep(reconnect_delay(&config, attempt)).await;
    }
}

fn derive_session_state(
    exchange_info: &ExchangeSymbol,
    schedule: &TradingSchedule,
    now_ms: i64,
) -> String {
    if !exchange_info.status.eq_ignore_ascii_case("TRADING") {
        return exchange_info.status.to_ascii_lowercase();
    }

    match exchange_info.underlying_type.as_str() {
        "EQUITY" | "COMMODITY" => schedule
            .market_schedules
            .get(exchange_info.underlying_type.as_str())
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|session| session.start_time_ms <= now_ms && now_ms < session.end_time_ms)
            })
            .map(|session| session.session_type.to_ascii_lowercase())
            .unwrap_or_else(|| "no_trading".into()),
        _ => "continuous".into(),
    }
}

fn reconnect_delay(config: &BinanceConfig, attempt: u32) -> Duration {
    let factor = 2u32.saturating_pow(attempt.saturating_sub(1)).max(1);
    let candidate = config
        .reconnect_base_delay
        .as_millis()
        .saturating_mul(factor as u128);
    let max = config.reconnect_max_delay.as_millis();
    Duration::from_millis(candidate.min(max).min(u64::MAX as u128) as u64)
}

fn timestamp_millis_to_rfc3339(timestamp_ms: i64) -> String {
    Utc.timestamp_millis_opt(timestamp_ms)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

async fn decode_response<T>(response: reqwest::Response) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read response body")?;
    decode_response_body(status, body)
}

fn decode_response_body<T>(status: reqwest::StatusCode, body: String) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    if status.is_success() {
        return serde_json::from_str(&body).context("failed to decode binance response");
    }
    anyhow::bail!("binance request failed with status {status}: {body}");
}

fn is_timestamp_outside_recv_window(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::BAD_REQUEST
        && serde_json::from_str::<BinanceErrorResponse>(body)
            .map(|error| error.code == -1021)
            .unwrap_or(false)
}

fn is_missing_order_error(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::BAD_REQUEST
        && serde_json::from_str::<BinanceErrorResponse>(body)
            .map(|error| error.code == -2011)
            .unwrap_or(false)
}

fn is_position_mode_mismatch_error(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::BAD_REQUEST
        && serde_json::from_str::<BinanceErrorResponse>(body)
            .map(|error| error.code == -4061)
            .unwrap_or(false)
}

fn build_submit_order_query(symbol: &str, request: &SubmitOrderRequest) -> String {
    let side = request.side.to_ascii_uppercase();
    if request.reduce_only {
        return format!(
            "symbol={symbol}&side={side}&type=MARKET&quantity={}&reduceOnly=true&newClientOrderId={}&newOrderRespType=RESULT",
            request.qty, request.client_order_id
        );
    }

    format!(
        "symbol={symbol}&side={side}&type=LIMIT&timeInForce=GTC&quantity={}&price={}&newClientOrderId={}&newOrderRespType=RESULT",
        request.qty, request.price, request.client_order_id
    )
}

fn build_cancel_order_query(
    symbol: &str,
    order_id: Option<&String>,
    client_order_id: Option<&String>,
) -> String {
    let mut query = format!("symbol={symbol}");
    if let Some(order_id) = order_id {
        query.push_str(&format!("&orderId={order_id}"));
    }
    if let Some(client_order_id) = client_order_id {
        query.push_str(&format!("&origClientOrderId={client_order_id}"));
    }
    query
}

fn decode_market_stream(payload: &str) -> Result<MarketStreamEvent> {
    let envelope: CombinedStreamEnvelope =
        serde_json::from_str(payload).context("failed to decode market stream envelope")?;
    if envelope.stream.contains("@aggTrade") {
        let event: AggTradePayload =
            serde_json::from_value(envelope.data).context("failed to decode aggTrade payload")?;
        return Ok(MarketStreamEvent {
            event_time_ms: event.event_time,
            last_price: Some(event.price),
            mark_price: None,
        });
    }

    let event: MarkPricePayload =
        serde_json::from_value(envelope.data).context("failed to decode markPrice payload")?;
    Ok(MarketStreamEvent {
        event_time_ms: event.event_time,
        last_price: None,
        mark_price: Some(event.mark_price),
    })
}

fn decode_user_stream(payload: &str) -> Result<Option<UserStreamEvent>> {
    let event: UserStreamEnvelope =
        serde_json::from_str(payload).context("failed to decode user stream payload")?;
    match event.event_type.as_str() {
        "ACCOUNT_UPDATE" => {
            let positions = event
                .account_update
                .map(|update| summarize_positions(update.positions))
                .unwrap_or_default();
            Ok(Some(UserStreamEvent {
                event_time_ms: event.event_time,
                positions,
                order_updates: vec![],
            }))
        }
        "ORDER_TRADE_UPDATE" => {
            let Some(order_trade_update) = event.order_trade_update else {
                return Ok(None);
            };
            Ok(Some(UserStreamEvent {
                event_time_ms: event.event_time,
                positions: vec![],
                order_updates: vec![translate_order_update(
                    order_trade_update.order,
                    event.event_time,
                )],
            }))
        }
        _ => Ok(None),
    }
}

fn translate_order_update(
    order: OrderTradeUpdateOrder,
    event_time_ms: i64,
) -> UserStreamOrderUpdate {
    let updated_at = timestamp_millis_to_rfc3339(event_time_ms);
    let status = order.order_status;
    UserStreamOrderUpdate {
        symbol: order.symbol,
        is_terminal: is_terminal_order_status(&status),
        order: OpenOrder {
            order_id: order.order_id.to_string(),
            client_order_id: order.client_order_id,
            side: order.side.to_ascii_lowercase(),
            price: order.price,
            qty: order.quantity,
            filled_qty: order.accumulated_filled_qty,
            status,
            created_at: updated_at.clone(),
            updated_at,
        },
    }
}

fn apply_order_update(open_orders: &mut Vec<OpenOrder>, update: UserStreamOrderUpdate) {
    if update.is_terminal {
        open_orders.retain(|current| {
            current.order_id != update.order.order_id
                && current.client_order_id != update.order.client_order_id
        });
        return;
    }

    if let Some(index) = open_orders.iter().position(|current| {
        current.order_id == update.order.order_id
            || current.client_order_id == update.order.client_order_id
    }) {
        open_orders[index] = update.order;
    } else {
        open_orders.push(update.order);
    }
}

fn is_terminal_order_status(status: &str) -> bool {
    matches!(
        status,
        "FILLED" | "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" | "REJECTED"
    )
}

fn summarize_positions(positions: Vec<AccountUpdatePosition>) -> Vec<PositionSnapshot> {
    let mut grouped = HashMap::<String, Vec<AccountUpdatePosition>>::new();
    for position in positions {
        grouped
            .entry(position.symbol.clone())
            .or_default()
            .push(position);
    }

    grouped
        .into_iter()
        .map(|(symbol, items)| {
            let mut total_qty = 0.0;
            let mut long_qty = 0.0;
            let mut short_qty = 0.0;
            let mut long_notional = 0.0;
            let mut short_notional = 0.0;
            let mut unrealized_pnl = 0.0;
            let mut realized_pnl = 0.0;
            for position in items {
                let qty = position.signed_position_amount();
                total_qty += qty;
                if qty > f64::EPSILON {
                    long_qty += qty;
                    long_notional += qty * position.entry_price;
                } else if qty < -f64::EPSILON {
                    short_qty += qty.abs();
                    short_notional += qty.abs() * position.entry_price;
                }
                unrealized_pnl += position.unrealized_pnl;
                realized_pnl += position.accumulated_realized;
            }
            let avg_price = if total_qty > f64::EPSILON && long_qty > f64::EPSILON {
                long_notional / long_qty
            } else if total_qty < -f64::EPSILON && short_qty > f64::EPSILON {
                short_notional / short_qty
            } else {
                0.0
            };
            PositionSnapshot {
                symbol,
                qty: total_qty,
                avg_price,
                unrealized_pnl,
                realized_pnl,
            }
        })
        .collect()
}

fn summarize_position_risk(
    positions: Vec<PositionRiskResponsePosition>,
) -> Option<PositionSnapshot> {
    let mut matching = positions.into_iter().filter(|position| {
        let qty = position.signed_position_amount();
        qty.abs() > f64::EPSILON
    });
    let first = matching.next()?;
    let symbol = first.symbol.clone();
    let mut total_qty = first.signed_position_amount();
    let mut long_qty = 0.0;
    let mut short_qty = 0.0;
    let mut long_notional = 0.0;
    let mut short_notional = 0.0;
    let mut unrealized_pnl = first.unrealized_pnl;

    if total_qty > f64::EPSILON {
        long_qty += total_qty;
        long_notional += total_qty * first.entry_price;
    } else if total_qty < -f64::EPSILON {
        short_qty += total_qty.abs();
        short_notional += total_qty.abs() * first.entry_price;
    }

    for position in matching {
        let qty = position.signed_position_amount();
        total_qty += qty;
        unrealized_pnl += position.unrealized_pnl;

        if qty > f64::EPSILON {
            long_qty += qty;
            long_notional += qty * position.entry_price;
        } else if qty < -f64::EPSILON {
            short_qty += qty.abs();
            short_notional += qty.abs() * position.entry_price;
        }
    }

    let avg_price = if total_qty > f64::EPSILON && long_qty > f64::EPSILON {
        long_notional / long_qty
    } else if total_qty < -f64::EPSILON && short_qty > f64::EPSILON {
        short_notional / short_qty
    } else {
        0.0
    };

    Some(PositionSnapshot {
        symbol,
        qty: total_qty,
        avg_price,
        unrealized_pnl,
        realized_pnl: 0.0,
    })
}

async fn sync_market_prices_until_applied(
    engine: &EngineHandle,
    event: MarketStreamEvent,
    retry_delay: Duration,
) {
    loop {
        match engine
            .sync_market_prices(
                event.last_price,
                event.mark_price,
                timestamp_millis_to_rfc3339(event.event_time_ms),
            )
            .await
        {
            Ok(()) => return,
            Err(error) => {
                warn!(
                    ?error,
                    "failed to persist binance market event; retrying same event"
                );
                sleep(retry_delay).await;
            }
        }
    }
}

async fn sync_runtime_patch_until_applied(
    engine: &EngineHandle,
    patch: RuntimePatch,
    retry_delay: Duration,
    context: &'static str,
) {
    loop {
        match engine.sync_runtime(patch.clone()).await {
            Ok(()) => return,
            Err(error) => {
                warn!(?error, %context, "failed to persist binance runtime patch; retrying");
                sleep(retry_delay).await;
            }
        }
    }
}

async fn sync_exchange_open_orders_until_applied(
    engine: &EngineHandle,
    orders: Vec<OpenOrder>,
    source: OpenOrdersSource,
    retry_delay: Duration,
    context: &'static str,
) {
    loop {
        match engine
            .sync_exchange_open_orders(orders.clone(), source)
            .await
        {
            Ok(()) => return,
            Err(error) => {
                warn!(
                    ?error,
                    %context,
                    "failed to persist exchange open orders; retrying"
                );
                sleep(retry_delay).await;
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ExchangeInfoResponse {
    symbols: Vec<ExchangeInfoSymbol>,
}

#[derive(Debug, Deserialize)]
struct ExchangeInfoSymbol {
    symbol: String,
    status: String,
    #[serde(rename = "underlyingType")]
    underlying_type: String,
    #[serde(rename = "pricePrecision")]
    price_precision: u32,
    #[serde(rename = "quantityPrecision")]
    quantity_precision: u32,
    #[serde(default)]
    filters: Vec<ExchangeInfoFilter>,
}

#[derive(Debug, Deserialize)]
struct ExchangeInfoFilter {
    #[serde(rename = "filterType")]
    filter_type: String,
    #[serde(
        rename = "tickSize",
        default,
        deserialize_with = "deserialize_optional_string_number"
    )]
    tick_size: Option<f64>,
    #[serde(
        rename = "minPrice",
        default,
        deserialize_with = "deserialize_optional_string_number"
    )]
    min_price: Option<f64>,
    #[serde(
        rename = "stepSize",
        default,
        deserialize_with = "deserialize_optional_string_number"
    )]
    step_size: Option<f64>,
    #[serde(
        rename = "minQty",
        default,
        deserialize_with = "deserialize_optional_string_number"
    )]
    min_qty: Option<f64>,
    #[serde(
        rename = "notional",
        default,
        deserialize_with = "deserialize_optional_string_number"
    )]
    notional: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct TradingScheduleResponse {
    #[serde(rename = "updateTime")]
    update_time: i64,
    #[serde(rename = "marketSchedules")]
    market_schedules: HashMap<String, MarketScheduleBlock>,
}

#[derive(Debug, Deserialize)]
struct MarketScheduleBlock {
    sessions: Vec<TradingScheduleSession>,
}

#[derive(Debug, Deserialize)]
struct TradingScheduleSession {
    #[serde(rename = "startTime")]
    start_time: i64,
    #[serde(rename = "endTime")]
    end_time: i64,
    #[serde(rename = "type")]
    session_type: String,
}

#[derive(Debug, Deserialize)]
struct CombinedStreamEnvelope {
    stream: String,
    data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AggTradePayload {
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "p", deserialize_with = "deserialize_string_number")]
    price: f64,
}

#[derive(Debug, Deserialize)]
struct MarkPricePayload {
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "p", deserialize_with = "deserialize_string_number")]
    mark_price: f64,
}

#[derive(Debug, Deserialize)]
struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    listen_key: String,
}

#[derive(Debug, Deserialize)]
struct ServerTimeResponse {
    #[serde(rename = "serverTime")]
    server_time_ms: i64,
}

#[derive(Debug, Deserialize)]
struct PositionModeResponse {
    #[serde(rename = "dualSidePosition")]
    dual_side_position: bool,
}

#[derive(Debug, Deserialize)]
struct BinanceErrorResponse {
    code: i64,
}

#[derive(Debug, Deserialize)]
struct SubmitOrderResponse {
    #[serde(rename = "orderId")]
    order_id: i64,
    #[serde(rename = "clientOrderId")]
    client_order_id: String,
    side: String,
    #[serde(rename = "price", deserialize_with = "deserialize_string_number")]
    price: f64,
    #[serde(rename = "origQty", deserialize_with = "deserialize_string_number")]
    orig_qty: f64,
    #[serde(rename = "executedQty", deserialize_with = "deserialize_string_number")]
    executed_qty: f64,
    status: String,
    #[serde(
        rename = "avgPrice",
        default,
        deserialize_with = "deserialize_optional_string_number"
    )]
    avg_price: Option<f64>,
    #[serde(rename = "updateTime", default)]
    update_time_ms: Option<i64>,
    #[serde(rename = "transactTime", default)]
    transact_time_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct UserStreamEnvelope {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "a")]
    account_update: Option<AccountUpdatePayload>,
    #[serde(rename = "o")]
    order_trade_update: Option<OrderTradeUpdatePayload>,
}

#[derive(Debug, Deserialize)]
struct AccountUpdatePayload {
    #[serde(rename = "P", default)]
    positions: Vec<AccountUpdatePosition>,
}

#[derive(Debug, Deserialize)]
struct AccountUpdatePosition {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "ps", default = "default_position_side")]
    position_side: String,
    #[serde(rename = "pa", deserialize_with = "deserialize_string_number")]
    position_amount: f64,
    #[serde(rename = "ep", deserialize_with = "deserialize_string_number")]
    entry_price: f64,
    #[serde(rename = "up", deserialize_with = "deserialize_string_number")]
    unrealized_pnl: f64,
    #[serde(rename = "cr", deserialize_with = "deserialize_string_number")]
    accumulated_realized: f64,
}

impl AccountUpdatePosition {
    fn signed_position_amount(&self) -> f64 {
        match self.position_side.as_str() {
            "LONG" => self.position_amount.abs(),
            "SHORT" => -self.position_amount.abs(),
            _ => self.position_amount,
        }
    }
}

#[derive(Debug, Deserialize)]
struct OrderTradeUpdatePayload {
    #[serde(flatten)]
    order: OrderTradeUpdateOrder,
}

#[derive(Debug, Deserialize)]
struct OrderTradeUpdateOrder {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "c")]
    client_order_id: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "q", deserialize_with = "deserialize_string_number")]
    quantity: f64,
    #[serde(rename = "p", deserialize_with = "deserialize_string_number")]
    price: f64,
    #[serde(rename = "X")]
    order_status: String,
    #[serde(rename = "i")]
    order_id: i64,
    #[serde(rename = "z", deserialize_with = "deserialize_string_number")]
    accumulated_filled_qty: f64,
}

#[derive(Debug, Deserialize)]
struct OpenOrdersResponseOrder {
    #[serde(rename = "orderId")]
    order_id: i64,
    #[serde(rename = "clientOrderId")]
    client_order_id: String,
    side: String,
    #[serde(rename = "price", deserialize_with = "deserialize_string_number")]
    price: f64,
    #[serde(rename = "origQty", deserialize_with = "deserialize_string_number")]
    orig_qty: f64,
    #[serde(rename = "executedQty", deserialize_with = "deserialize_string_number")]
    executed_qty: f64,
    status: String,
    #[serde(rename = "time")]
    created_at_ms: i64,
    #[serde(rename = "updateTime")]
    updated_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct PositionRiskResponsePosition {
    symbol: String,
    #[serde(rename = "positionSide", default = "default_position_side")]
    position_side: String,
    #[serde(rename = "positionAmt", deserialize_with = "deserialize_string_number")]
    position_amount: f64,
    #[serde(rename = "entryPrice", deserialize_with = "deserialize_string_number")]
    entry_price: f64,
    #[serde(
        rename = "unRealizedProfit",
        deserialize_with = "deserialize_string_number"
    )]
    unrealized_pnl: f64,
}

impl PositionRiskResponsePosition {
    fn signed_position_amount(&self) -> f64 {
        match self.position_side.as_str() {
            "LONG" => self.position_amount.abs(),
            "SHORT" => -self.position_amount.abs(),
            _ => self.position_amount,
        }
    }
}

impl OpenOrdersResponseOrder {
    fn into_open_order(self) -> OpenOrder {
        OpenOrder {
            order_id: self.order_id.to_string(),
            client_order_id: self.client_order_id,
            side: self.side.to_ascii_lowercase(),
            price: self.price,
            qty: self.orig_qty,
            filled_qty: self.executed_qty,
            status: self.status,
            created_at: timestamp_millis_to_rfc3339(self.created_at_ms),
            updated_at: timestamp_millis_to_rfc3339(self.updated_at_ms),
        }
    }
}

impl ExchangeInfoSymbol {
    fn into_order_rules(&self) -> Option<ExchangeOrderRules> {
        let price_filter = self
            .filters
            .iter()
            .find(|filter| filter.filter_type == "PRICE_FILTER")?;
        let lot_size_filter = self
            .filters
            .iter()
            .find(|filter| filter.filter_type == "LOT_SIZE")?;
        let min_notional = self
            .filters
            .iter()
            .find(|filter| filter.filter_type == "MIN_NOTIONAL")
            .and_then(|filter| filter.notional)
            .unwrap_or(0.0);

        Some(ExchangeOrderRules {
            price_tick: price_filter.tick_size.unwrap_or(0.0),
            price_precision: self.price_precision,
            min_price: price_filter.min_price.unwrap_or(0.0),
            quantity_step: lot_size_filter.step_size.unwrap_or(0.0),
            quantity_precision: self.quantity_precision,
            min_qty: lot_size_filter.min_qty.unwrap_or(0.0),
            min_notional,
        })
    }
}

impl SubmitOrderResponse {
    fn into_submit_result(self, request: &SubmitOrderRequest) -> SubmitOrderResult {
        let updated_at_ms = self
            .update_time_ms
            .or(self.transact_time_ms)
            .unwrap_or_else(|| Utc::now().timestamp_millis());
        let updated_at = timestamp_millis_to_rfc3339(updated_at_ms);
        let open_order = (!is_terminal_order_status(&self.status)).then(|| OpenOrder {
            order_id: self.order_id.to_string(),
            client_order_id: self.client_order_id.clone(),
            side: self.side.to_ascii_lowercase(),
            price: if self.price > f64::EPSILON {
                self.price
            } else {
                request.price
            },
            qty: self.orig_qty,
            filled_qty: self.executed_qty,
            status: self.status.clone(),
            created_at: updated_at.clone(),
            updated_at: updated_at.clone(),
        });
        let fill = (self.executed_qty > f64::EPSILON).then(|| RecentFill {
            trade_id: format!("binance_{}_{}", self.order_id, updated_at_ms),
            order_id: self.order_id.to_string(),
            client_order_id: Some(self.client_order_id.clone()),
            side: self.side.to_ascii_lowercase(),
            price: self
                .avg_price
                .filter(|price| *price > f64::EPSILON)
                .unwrap_or_else(|| {
                    if self.price > f64::EPSILON {
                        self.price
                    } else {
                        request.price
                    }
                }),
            qty: self.executed_qty,
            fee: 0.0,
            realized_pnl: 0.0,
            event_time: updated_at,
        });

        SubmitOrderResult { open_order, fill }
    }
}

fn sign_query(query: &str, api_secret: &str) -> Result<String> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(api_secret.as_bytes()).context("invalid hmac key")?;
    mac.update(query.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn default_position_side() -> String {
    "BOTH".into()
}

fn deserialize_string_number<'de, D>(deserializer: D) -> std::result::Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    value.parse::<f64>().map_err(serde::de::Error::custom)
}

fn deserialize_optional_string_number<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| value.parse::<f64>().map_err(serde::de::Error::custom))
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use axum::{
        Json, Router,
        extract::{Query, State},
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
    };
    use std::fs;
    use tokio::sync::{Mutex, mpsc};

    use tempfile::tempdir;

    use super::*;
    use crate::{
        kernel::spawn_engine_with_runtime,
        storage::{PersistedRuntime, SqliteStorage},
    };

    #[derive(Clone)]
    struct SignedRequestTestState {
        server_time_ms: i64,
        time_requests: Arc<AtomicUsize>,
        cancel_requests: Arc<AtomicUsize>,
    }

    impl SignedRequestTestState {
        fn new(server_time_ms: i64) -> Self {
            Self {
                server_time_ms,
                time_requests: Arc::new(AtomicUsize::new(0)),
                cancel_requests: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    async fn mock_server_time(
        State(state): State<SignedRequestTestState>,
    ) -> Json<serde_json::Value> {
        state.time_requests.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({
            "serverTime": state.server_time_ms,
        }))
    }

    async fn mock_open_orders(
        State(state): State<SignedRequestTestState>,
        Query(params): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        let timestamp = params
            .get("timestamp")
            .and_then(|value| value.parse::<i64>().ok());
        let recv_window_ms = params
            .get("recvWindow")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(5_000)
            .min(60_000);

        let Some(timestamp) = timestamp else {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"code":-1022,"msg":"Signature for this request is not valid."}"#,
            )
                .into_response();
        };

        if (state.server_time_ms - timestamp).abs() > recv_window_ms {
            return (
                StatusCode::BAD_REQUEST,
                r#"{"code":-1021,"msg":"Timestamp for this request is outside of the recvWindow."}"#,
            )
                .into_response();
        }

        Json(serde_json::json!([
            {
                "orderId": 42,
                "clientOrderId": "grid-order-42",
                "side": "BUY",
                "price": "3100.5",
                "origQty": "0.25",
                "executedQty": "0.05",
                "status": "NEW",
                "time": 1_710_000_000_000_i64,
                "updateTime": 1_710_000_060_000_i64
            }
        ]))
        .into_response()
    }

    async fn mock_cancel_order(
        State(state): State<SignedRequestTestState>,
        Query(_params): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        state.cancel_requests.fetch_add(1, Ordering::SeqCst);
        (
            StatusCode::BAD_REQUEST,
            r#"{"code":-2011,"msg":"Unknown order sent."}"#,
        )
            .into_response()
    }

    async fn mock_submit_order_hedge_mode() -> impl IntoResponse {
        (
            StatusCode::BAD_REQUEST,
            r#"{"code":-4061,"msg":"Order's position side does not match user's setting."}"#,
        )
            .into_response()
    }

    async fn spawn_signed_request_test_server(
        state: SignedRequestTestState,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let app = Router::new()
            .route("/fapi/v1/time", get(mock_server_time))
            .route("/fapi/v1/openOrders", get(mock_open_orders))
            .route(
                "/fapi/v1/order",
                post(mock_submit_order_hedge_mode).delete(mock_cancel_order),
            )
            .with_state(state);
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve test server");
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn summarize_positions_uses_directional_average_for_hedged_exposure() {
        let positions = summarize_positions(vec![
            AccountUpdatePosition {
                symbol: "XAUUSDT".into(),
                position_side: "LONG".into(),
                position_amount: 1.0,
                entry_price: 100.0,
                unrealized_pnl: 10.0,
                accumulated_realized: 3.0,
            },
            AccountUpdatePosition {
                symbol: "XAUUSDT".into(),
                position_side: "SHORT".into(),
                position_amount: -0.5,
                entry_price: 110.0,
                unrealized_pnl: -2.0,
                accumulated_realized: 1.0,
            },
        ]);

        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].symbol, "XAUUSDT");
        assert_eq!(positions[0].qty, 0.5);
        assert_eq!(positions[0].avg_price, 100.0);
        assert_eq!(positions[0].unrealized_pnl, 8.0);
        assert_eq!(positions[0].realized_pnl, 4.0);
    }

    #[derive(Clone, Default)]
    struct MarketOnlyTransport {
        market_streams: Arc<Mutex<VecDeque<mpsc::Receiver<MarketStreamEvent>>>>,
    }

    impl MarketOnlyTransport {
        async fn push_market_stream(&self, receiver: mpsc::Receiver<MarketStreamEvent>) {
            self.market_streams.lock().await.push_back(receiver);
        }
    }

    #[async_trait]
    impl BinanceTransport for MarketOnlyTransport {
        async fn fetch_exchange_info(&self, _symbol: &str) -> Result<ExchangeSymbol> {
            Err(anyhow!("unused in this test"))
        }

        async fn fetch_trading_schedule(&self) -> Result<TradingSchedule> {
            Err(anyhow!("unused in this test"))
        }

        async fn connect_market_stream(
            &self,
            _symbol: &str,
        ) -> Result<mpsc::Receiver<MarketStreamEvent>> {
            self.market_streams
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted market stream available"))
        }

        async fn create_user_stream(&self) -> Result<Option<String>> {
            Ok(None)
        }

        async fn connect_user_stream(
            &self,
            _listen_key: &str,
        ) -> Result<mpsc::Receiver<UserStreamEvent>> {
            Err(anyhow!("unused in this test"))
        }

        async fn keepalive_user_stream(&self, _listen_key: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_reporter_reverts_local_state_when_sync_fails() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("service.db");
        let storage = SqliteStorage::open(&db_path).expect("sqlite open");
        let runtime = PersistedRuntime::sqlite_bootstrap();
        let (engine, read_model, _events_rx) =
            spawn_engine_with_runtime(runtime.clone(), Some(storage.clone()));
        let reporter = ConnectionReporter::new(engine, runtime.snapshot.connection.clone());

        fs::remove_file(&db_path).expect("remove sqlite file");
        fs::create_dir(&db_path).expect("replace sqlite file with directory");

        assert!(
            reporter
                .update(|state| state.user_stream_connected = Some(true))
                .await
                .is_err()
        );

        fs::remove_dir(&db_path).expect("remove sqlite directory");
        SqliteStorage::open(&db_path).expect("recreate sqlite db");

        reporter
            .update(|state| state.stale_age_ms = 42)
            .await
            .expect("connection sync should recover after sqlite is restored");

        let snapshot = read_model.read().expect("read_model").snapshot();
        assert_eq!(snapshot.connection.user_stream_connected, None);
        assert_eq!(snapshot.connection.stale_age_ms, 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn market_connect_retries_connection_state_and_clears_old_heartbeat() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("service.db");
        let storage = SqliteStorage::open(&db_path).expect("sqlite open");
        let runtime = PersistedRuntime::sqlite_bootstrap();
        let (engine, read_model, _events_rx) =
            spawn_engine_with_runtime(runtime.clone(), Some(storage.clone()));
        let reporter = ConnectionReporter::new(engine.clone(), runtime.snapshot.connection.clone());
        let last_market_heartbeat = Arc::new(Mutex::new(MarketHeartbeatState::Disconnected));
        let transport = MarketOnlyTransport::default();
        let (_market_tx, market_rx) = mpsc::channel(4);
        transport.push_market_stream(market_rx).await;

        let mut config = BinanceConfig::testnet("XAUUSDT");
        config.reconnect_base_delay = Duration::from_millis(40);
        config.reconnect_max_delay = Duration::from_millis(40);

        fs::remove_file(&db_path).expect("remove sqlite file");
        fs::create_dir(&db_path).expect("replace sqlite file with directory");

        let task = tokio::spawn(run_market_loop(
            engine,
            reporter,
            config,
            Arc::new(transport),
            last_market_heartbeat,
        ));

        tokio::time::sleep(Duration::from_millis(120)).await;
        fs::remove_dir(&db_path).expect("remove sqlite directory");
        SqliteStorage::open(&db_path).expect("recreate sqlite db");

        let observed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let snapshot = read_model.read().expect("read_model").snapshot();
                if snapshot.connection.ws_connected
                    && snapshot.connection.last_heartbeat_at.is_empty()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;

        task.abort();
        assert!(
            observed.is_ok(),
            "market connect should retry connection state sync and clear old heartbeat"
        );
    }

    #[tokio::test]
    async fn signed_open_orders_syncs_server_time_when_local_clock_is_skewed() {
        let state = SignedRequestTestState::new(Utc::now().timestamp_millis() + 90_000);
        let (rest_base_url, server) = spawn_signed_request_test_server(state.clone()).await;

        let mut config = BinanceConfig::testnet("ETHUSDT");
        config.rest_base_url = rest_base_url;
        config.api_key = Some("test-api-key".into());
        config.api_secret = Some("test-api-secret".into());

        let transport = RealBinanceTransport::new(&config).expect("transport");
        let orders = transport
            .fetch_open_orders("ETHUSDT")
            .await
            .expect("signed request should recover after syncing server time")
            .expect("open orders should be available");

        server.abort();

        assert_eq!(state.time_requests.load(Ordering::SeqCst), 1);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_id, "42");
        assert_eq!(orders[0].client_order_id, "grid-order-42");
        assert_eq!(orders[0].side, "buy");
    }

    #[tokio::test]
    async fn cancel_orders_treats_missing_order_response_as_success() {
        let state = SignedRequestTestState::new(Utc::now().timestamp_millis());
        let (rest_base_url, server) = spawn_signed_request_test_server(state.clone()).await;

        let mut config = BinanceConfig::testnet("ETHUSDT");
        config.rest_base_url = rest_base_url;
        config.api_key = Some("test-api-key".into());
        config.api_secret = Some("test-api-secret".into());

        let transport = RealBinanceTransport::new(&config).expect("transport");
        let mut snapshot = PersistedRuntime::sqlite_bootstrap().snapshot;
        snapshot.runtime.symbol = "ETHUSDT".into();

        let open_orders = transport
            .cancel_orders(
                CancelOrdersRequest {
                    command_id: Some("cmd_cancel_missing".into()),
                    order_ids: vec!["42".into()],
                    client_order_ids: Vec::new(),
                },
                &snapshot,
            )
            .await
            .expect("missing-order cancel should still succeed");

        server.abort();

        assert_eq!(state.cancel_requests.load(Ordering::SeqCst), 1);
        assert_eq!(open_orders.len(), 1);
        assert_eq!(open_orders[0].order_id, "42");
    }

    #[tokio::test]
    async fn submit_order_surfaces_hedge_mode_mismatch_with_actionable_message() {
        let state = SignedRequestTestState::new(Utc::now().timestamp_millis());
        let (rest_base_url, server) = spawn_signed_request_test_server(state).await;

        let mut config = BinanceConfig::testnet("ETHUSDT");
        config.rest_base_url = rest_base_url;
        config.api_key = Some("test-api-key".into());
        config.api_secret = Some("test-api-secret".into());

        let transport = RealBinanceTransport::new(&config).expect("transport");
        let mut snapshot = PersistedRuntime::sqlite_bootstrap().snapshot;
        snapshot.runtime.symbol = "ETHUSDT".into();

        let error = transport
            .submit_order(
                SubmitOrderRequest {
                    command_id: Some("cmd_submit_hedge".into()),
                    order_id: "ord_001".into(),
                    client_order_id: "grid_buy_01".into(),
                    side: "buy".into(),
                    price: 3000.0,
                    qty: 0.1,
                    reduce_only: false,
                },
                &snapshot,
            )
            .await
            .expect_err("hedge-mode mismatch should be surfaced as an actionable error");

        server.abort();

        assert!(error.to_string().contains("hedge mode"));
        assert!(error.to_string().contains("one-way mode"));
    }
}
