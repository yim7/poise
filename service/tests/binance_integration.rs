use std::{
    collections::{HashMap, VecDeque},
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use grid_platform_service::{
    Application,
    execution::{CancelOrdersRequest, SubmitOrderRequest, SubmitOrderResult},
    integrations::binance::{
        BinanceConfig, BinanceTransport, ExchangeSymbol, MarketStreamEvent, PositionSnapshot,
        TradingSchedule, TradingSession, UserStreamEvent, UserStreamFill, UserStreamOrderUpdate,
    },
    protocol::{
        CommandRequest, CommandStatus, CommandType, ExchangeOrderRules, GridConfig, OpenOrder,
        OpenOrdersSource, RecentFill, RuntimeSnapshot, SystemEvent,
    },
    storage::{PersistedRuntime, SqliteStorage},
};
use tempfile::tempdir;
use tokio::{
    sync::{Mutex, Notify, mpsc},
    time::sleep,
};

#[derive(Clone)]
struct FakeBinanceTransport {
    exchange_info: ExchangeSymbol,
    trading_schedule: TradingSchedule,
    market_streams: Arc<Mutex<VecDeque<mpsc::Receiver<MarketStreamEvent>>>>,
    user_streams: Arc<Mutex<VecDeque<mpsc::Receiver<UserStreamEvent>>>>,
    market_connects: Arc<AtomicUsize>,
    user_connects: Arc<AtomicUsize>,
    keepalive_calls: Arc<AtomicUsize>,
    submit_calls: Arc<AtomicUsize>,
    submit_requests: Arc<Mutex<Vec<SubmitOrderRequest>>>,
    cancel_calls: Arc<AtomicUsize>,
    cancel_result: Arc<Mutex<Option<Vec<OpenOrder>>>>,
    execution_enabled: bool,
    listen_key: Option<String>,
    open_orders: Option<Vec<OpenOrder>>,
    reduce_only_submit_gate: Option<Arc<Notify>>,
    reduce_only_submit_blocked: Arc<std::sync::atomic::AtomicBool>,
}

impl FakeBinanceTransport {
    fn new(exchange_info: ExchangeSymbol, trading_schedule: TradingSchedule) -> Self {
        Self {
            exchange_info,
            trading_schedule,
            market_streams: Arc::new(Mutex::new(VecDeque::new())),
            user_streams: Arc::new(Mutex::new(VecDeque::new())),
            market_connects: Arc::new(AtomicUsize::new(0)),
            user_connects: Arc::new(AtomicUsize::new(0)),
            keepalive_calls: Arc::new(AtomicUsize::new(0)),
            submit_calls: Arc::new(AtomicUsize::new(0)),
            submit_requests: Arc::new(Mutex::new(Vec::new())),
            cancel_calls: Arc::new(AtomicUsize::new(0)),
            cancel_result: Arc::new(Mutex::new(None)),
            execution_enabled: false,
            listen_key: None,
            open_orders: None,
            reduce_only_submit_gate: None,
            reduce_only_submit_blocked: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    async fn push_market_stream(&self, receiver: mpsc::Receiver<MarketStreamEvent>) {
        self.market_streams.lock().await.push_back(receiver);
    }

    async fn push_user_stream(&self, receiver: mpsc::Receiver<UserStreamEvent>) {
        self.user_streams.lock().await.push_back(receiver);
    }

    fn with_listen_key(mut self, listen_key: impl Into<String>) -> Self {
        self.listen_key = Some(listen_key.into());
        self
    }

    fn with_open_orders(mut self, open_orders: Vec<OpenOrder>) -> Self {
        self.open_orders = Some(open_orders);
        self
    }

    fn with_execution_enabled(mut self) -> Self {
        self.execution_enabled = true;
        self
    }

    fn with_reduce_only_submit_gate(mut self, gate: Arc<Notify>) -> Self {
        self.reduce_only_submit_gate = Some(gate);
        self
    }

    fn market_connects(&self) -> usize {
        self.market_connects.load(Ordering::SeqCst)
    }

    fn keepalive_calls(&self) -> usize {
        self.keepalive_calls.load(Ordering::SeqCst)
    }

    fn submit_calls(&self) -> usize {
        self.submit_calls.load(Ordering::SeqCst)
    }

    async fn submit_requests(&self) -> Vec<SubmitOrderRequest> {
        self.submit_requests.lock().await.clone()
    }

    fn cancel_calls(&self) -> usize {
        self.cancel_calls.load(Ordering::SeqCst)
    }

    fn reduce_only_submit_blocked(&self) -> bool {
        self.reduce_only_submit_blocked.load(Ordering::SeqCst)
    }

    async fn set_cancel_result(&self, orders: Vec<OpenOrder>) {
        *self.cancel_result.lock().await = Some(orders);
    }
}

#[async_trait]
impl BinanceTransport for FakeBinanceTransport {
    async fn fetch_exchange_info(&self, symbol: &str) -> Result<ExchangeSymbol> {
        if self.exchange_info.symbol != symbol {
            anyhow::bail!("unexpected symbol {symbol}");
        }
        Ok(self.exchange_info.clone())
    }

    async fn fetch_trading_schedule(&self) -> Result<TradingSchedule> {
        Ok(self.trading_schedule.clone())
    }

    fn supports_execution(&self) -> bool {
        self.execution_enabled
    }

    async fn connect_market_stream(
        &self,
        _symbol: &str,
    ) -> Result<mpsc::Receiver<MarketStreamEvent>> {
        self.market_connects.fetch_add(1, Ordering::SeqCst);
        self.market_streams
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| anyhow!("no scripted market stream available"))
    }

    async fn create_user_stream(&self) -> Result<Option<String>> {
        Ok(self.listen_key.clone())
    }

    async fn connect_user_stream(
        &self,
        _listen_key: &str,
    ) -> Result<mpsc::Receiver<UserStreamEvent>> {
        self.user_connects.fetch_add(1, Ordering::SeqCst);
        self.user_streams
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| anyhow!("no scripted user stream available"))
    }

    async fn keepalive_user_stream(&self, _listen_key: &str) -> Result<()> {
        self.keepalive_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn fetch_open_orders(&self, _symbol: &str) -> Result<Option<Vec<OpenOrder>>> {
        Ok(self.open_orders.clone())
    }

    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        _snapshot: &RuntimeSnapshot,
    ) -> Result<SubmitOrderResult> {
        self.submit_calls.fetch_add(1, Ordering::SeqCst);
        self.submit_requests.lock().await.push(request.clone());
        if request.reduce_only {
            if let Some(gate) = &self.reduce_only_submit_gate {
                self.reduce_only_submit_blocked
                    .store(true, Ordering::SeqCst);
                gate.notified().await;
                self.reduce_only_submit_blocked
                    .store(false, Ordering::SeqCst);
            }
            return Ok(SubmitOrderResult {
                open_order: None,
                fill: Some(RecentFill {
                    trade_id: format!("binance_rest_{}_1735689600000", request.order_id),
                    order_id: request.order_id,
                    client_order_id: Some(request.client_order_id),
                    side: request.side,
                    price: request.price,
                    qty: request.qty,
                    fee: 0.0,
                    realized_pnl: 0.0,
                    event_time: "2025-01-01T00:00:00Z".into(),
                }),
            });
        }

        Ok(SubmitOrderResult {
            open_order: Some(OpenOrder {
                order_id: request.order_id,
                client_order_id: request.client_order_id,
                side: request.side,
                price: request.price,
                qty: request.qty,
                filled_qty: 0.0,
                status: "NEW".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                updated_at: "2025-01-01T00:00:00Z".into(),
            }),
            fill: None,
        })
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<Vec<OpenOrder>> {
        self.cancel_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(result) = self.cancel_result.lock().await.take() {
            return Ok(result);
        }
        Ok(snapshot
            .execution
            .open_orders
            .iter()
            .filter(|order| {
                !request.order_ids.iter().any(|id| id == &order.order_id)
                    && !request
                        .client_order_ids
                        .iter()
                        .any(|id| id == &order.client_order_id)
            })
            .cloned()
            .collect())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_binance_market_sync_updates_session_prices_stale_and_reconnects() -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    );
    let (market_tx_1, market_rx_1) = mpsc::channel(16);
    let (market_tx_2, market_rx_2) = mpsc::channel(16);
    transport.push_market_stream(market_rx_1).await;
    transport.push_market_stream(market_rx_2).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_binance(config, Arc::new(transport.clone()));

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.runtime.env == "testnet"
            && snapshot.runtime.session_state == "regular"
            && snapshot.connection.http_available
    })
    .await
    .context("metadata bootstrap did not complete")?;

    wait_until(Duration::from_secs(2), || {
        app.snapshot().connection.ws_connected
    })
    .await
    .context("market websocket did not connect before first event")?;
    wait_until(Duration::from_secs(2), || {
        app.snapshot().connection.stale_age_ms >= 50
    })
    .await
    .context("stale age did not advance before the first market event")?;

    market_tx_1
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 1_000,
            last_price: Some(2350.12),
            mark_price: None,
        })
        .await
        .context("failed to send first market trade")?;
    market_tx_1
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 1_200,
            last_price: None,
            mark_price: Some(2350.45),
        })
        .await
        .context("failed to send first market mark price")?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.ws_connected
            && snapshot.runtime.last_price == 2350.12
            && snapshot.runtime.mark_price == 2350.45
    })
    .await
    .context("first market prices did not reach runtime snapshot")?;
    wait_until(Duration::from_secs(2), || {
        app.snapshot().connection.stale_age_ms >= 75
    })
    .await
    .context("stale age did not advance after the first market event")?;
    let stale_before_disconnect = app.snapshot().connection.stale_age_ms;

    drop(market_tx_1);

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        !snapshot.connection.ws_connected && snapshot.connection.reconnect_backoff_ms >= 40
    })
    .await
    .context("market disconnect did not move into reconnecting state")?;
    wait_until(Duration::from_secs(2), || transport.market_connects() >= 2)
        .await
        .context("market reconnect did not create a second stream attempt")?;
    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.ws_connected
            && snapshot.connection.stale_age_ms < stale_before_disconnect
    })
    .await
    .context("reconnected market stream inherited stale age from the previous connection")?;
    assert!(
        app.snapshot().connection.last_heartbeat_at.is_empty(),
        "reconnected market stream should clear stale heartbeat until a new event arrives"
    );

    market_tx_2
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 2_000,
            last_price: Some(2351.0),
            mark_price: Some(2351.3),
        })
        .await
        .context("failed to send second market event")?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.ws_connected
            && snapshot.connection.stale_age_ms < 50
            && snapshot.runtime.last_price == 2351.0
            && snapshot.runtime.mark_price == 2351.3
    })
    .await
    .context("second market stream did not recover runtime prices")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binance_bootstrap_without_sqlite_clears_sample_runtime_before_background_sync()
-> Result<()> {
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms_or_zero(),
            market_schedules: HashMap::new(),
        },
    );

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.metadata_refresh_interval = Duration::from_secs(3600);
    config.health_tick_interval = Duration::from_secs(3600);
    config.reconnect_base_delay = Duration::from_secs(3600);
    config.reconnect_max_delay = Duration::from_secs(3600);

    let app = Application::bootstrap_with_binance(config, Arc::new(transport));
    let snapshot = app.snapshot();

    assert_eq!(snapshot.runtime.symbol, "XAUUSDT");
    assert_eq!(snapshot.runtime.env, "testnet");
    assert_eq!(snapshot.runtime.session_state, "syncing");
    assert_eq!(snapshot.runtime.last_price, 0.0);
    assert_eq!(snapshot.runtime.mark_price, 0.0);
    assert_eq!(snapshot.runtime.position_qty, 0.0);
    assert_eq!(snapshot.runtime.position_avg_price, 0.0);
    assert_eq!(snapshot.runtime.unrealized_pnl, 0.0);
    assert_eq!(snapshot.runtime.realized_pnl, 0.0);
    assert!(!snapshot.connection.http_available);
    assert!(!snapshot.connection.ws_connected);
    assert_eq!(snapshot.connection.user_stream_connected, None);
    assert!(snapshot.connection.last_heartbeat_at.is_empty());
    assert!(snapshot.execution.open_orders.is_empty());
    assert!(snapshot.execution.recent_fills.is_empty());
    assert!(snapshot.execution.pending_commands.is_empty());
    assert!(snapshot.execution.recent_commands.is_empty());
    assert!(snapshot.execution.last_command_ack.is_none());
    assert!(snapshot.execution.last_command_ack_event.is_none());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_binance_sync_persists_latest_runtime_for_recovery() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_listen_key("listen-key-persist");
    let (market_tx, market_rx) = mpsc::channel(16);
    let (user_tx, user_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;
    transport.push_user_stream(user_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.user_stream_keepalive_interval = Duration::from_millis(30);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_sqlite_and_binance(
        &db_path,
        config,
        Arc::new(transport.clone()),
    )?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.http_available
            && snapshot.connection.user_stream_connected == Some(true)
    })
    .await
    .context("binance sqlite bootstrap did not establish metadata and user stream state")?;

    market_tx
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 1_000,
            last_price: Some(2370.5),
            mark_price: Some(2370.9),
        })
        .await
        .context("failed to send persisted market event")?;
    user_tx
        .send(UserStreamEvent {
            event_time_ms: now_ms + 1_500,
            positions: vec![PositionSnapshot {
                symbol: "XAUUSDT".into(),
                qty: 2.5,
                avg_price: 2369.2,
                unrealized_pnl: 18.4,
                realized_pnl: 7.1,
            }],
            order_updates: vec![],
            recent_fills: vec![],
        })
        .await
        .context("failed to send persisted user event")?;

    wait_until(Duration::from_secs(2), || {
        let recovered = SqliteStorage::open(&db_path)
            .ok()
            .and_then(|storage| storage.load_runtime().ok().flatten());
        recovered.is_some_and(|runtime| {
            runtime.snapshot.runtime.last_price == 2370.5
                && runtime.snapshot.runtime.mark_price == 2370.9
                && runtime.snapshot.runtime.position_qty == 2.5
                && runtime.snapshot.runtime.position_avg_price == 2369.2
                && runtime.snapshot.connection.ws_connected
                && runtime.snapshot.connection.user_stream_connected == Some(true)
        })
    })
    .await
    .context("sqlite runtime did not persist the latest binance-derived state")?;

    let recovered = Application::bootstrap_with_sqlite(&db_path)?;
    let snapshot = recovered.snapshot();
    assert_eq!(snapshot.runtime.last_price, 2370.5);
    assert_eq!(snapshot.runtime.mark_price, 2370.9);
    assert_eq!(snapshot.runtime.position_qty, 2.5);
    assert_eq!(snapshot.runtime.position_avg_price, 2369.2);
    assert!(snapshot.connection.ws_connected);
    assert_eq!(snapshot.connection.user_stream_connected, Some(true));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_binance_bootstrap_seeds_runtime_config_before_background_sync() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;

    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.runtime.symbol = "XAUUSDT".into();
    snapshot.runtime.env = "testnet".into();
    snapshot.connection.http_available = true;
    snapshot.connection.ws_connected = true;
    snapshot.connection.user_stream_connected = Some(true);
    snapshot.runtime.last_price = 2450.0;
    snapshot.runtime.position_qty = 1.0;
    storage.persist_runtime(&PersistedRuntime {
        snapshot,
        risk_events: vec![],
        system_events: vec![SystemEvent {
            level: "info".into(),
            source: "bootstrap".into(),
            code: None,
            message: "persisted runtime".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
        }],
        last_sequence: 3,
    })?;

    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "BTCUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "CRYPTO".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms_or_zero(),
            market_schedules: HashMap::new(),
        },
    );

    let mut config = BinanceConfig::mainnet("BTCUSDT");
    config.api_key = Some("demo-key".into());
    config.metadata_refresh_interval = Duration::from_secs(3600);
    config.health_tick_interval = Duration::from_secs(3600);

    let app =
        Application::bootstrap_with_sqlite_and_binance(&db_path, config, Arc::new(transport))?;
    let snapshot = app.snapshot();

    assert_eq!(snapshot.runtime.symbol, "BTCUSDT");
    assert_eq!(snapshot.runtime.env, "mainnet");
    assert!(!snapshot.connection.http_available);
    assert!(!snapshot.connection.ws_connected);
    assert_eq!(snapshot.connection.user_stream_connected, Some(false));
    assert_eq!(snapshot.runtime.last_price, 0.0);
    assert_eq!(snapshot.runtime.position_qty, 0.0);
    assert_eq!(snapshot.runtime.session_state, "syncing");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_binance_stream_sync_retries_after_temporary_persist_failure() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_listen_key("listen-key-retry");
    let (market_tx, market_rx) = mpsc::channel(16);
    let (user_tx, user_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;
    transport.push_user_stream(user_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.user_stream_keepalive_interval = Duration::from_millis(30);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_sqlite_and_binance(
        &db_path,
        config,
        Arc::new(transport.clone()),
    )?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.http_available
            && snapshot.connection.ws_connected
            && snapshot.connection.user_stream_connected == Some(true)
    })
    .await
    .context("binance streams did not become ready before persistence failure test")?;

    fs::remove_file(&db_path)?;
    fs::create_dir(&db_path)?;

    market_tx
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 2_000,
            last_price: Some(2381.25),
            mark_price: Some(2381.75),
        })
        .await
        .context("failed to send market event during persistence outage")?;
    user_tx
        .send(UserStreamEvent {
            event_time_ms: now_ms + 2_100,
            positions: vec![PositionSnapshot {
                symbol: "XAUUSDT".into(),
                qty: 1.75,
                avg_price: 2380.5,
                unrealized_pnl: 12.4,
                realized_pnl: 4.8,
            }],
            order_updates: vec![],
            recent_fills: vec![],
        })
        .await
        .context("failed to send user stream event during persistence outage")?;

    sleep(Duration::from_millis(150)).await;
    let during_outage = app.snapshot();
    assert_ne!(during_outage.runtime.last_price, 2381.25);
    assert_ne!(during_outage.runtime.position_qty, 1.75);

    fs::remove_dir(&db_path)?;
    wait_until(Duration::from_secs(2), || {
        SqliteStorage::open(&db_path).is_ok()
    })
    .await
    .context("sqlite db was not recreatable after persistence recovered")?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.runtime.last_price == 2381.25
            && snapshot.runtime.mark_price == 2381.75
            && snapshot.runtime.position_qty == 1.75
            && snapshot.runtime.position_avg_price == 2380.5
    })
    .await
    .context("stream updates were not retried after sqlite persistence recovered")?;

    let persisted = SqliteStorage::open(&db_path)?
        .load_runtime()?
        .expect("runtime should be persisted after recovery");
    assert_eq!(persisted.snapshot.runtime.last_price, 2381.25);
    assert_eq!(persisted.snapshot.runtime.mark_price, 2381.75);
    assert_eq!(persisted.snapshot.runtime.position_qty, 1.75);
    assert_eq!(persisted.snapshot.runtime.position_avg_price, 2380.5);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_binance_user_stream_updates_position_snapshot() -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_listen_key("listen-key-001");
    let (_market_tx, market_rx) = mpsc::channel(16);
    let (user_tx, user_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;
    transport.push_user_stream(user_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.user_stream_keepalive_interval = Duration::from_millis(30);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_binance(config, Arc::new(transport.clone()));

    wait_until(Duration::from_secs(2), || {
        app.snapshot().connection.user_stream_connected == Some(true)
    })
    .await?;

    user_tx
        .send(UserStreamEvent {
            event_time_ms: now_ms + 2_000,
            positions: vec![
                PositionSnapshot {
                    symbol: "BTCUSDT".into(),
                    qty: 0.1,
                    avg_price: 80_000.0,
                    unrealized_pnl: 20.0,
                    realized_pnl: 5.0,
                },
                PositionSnapshot {
                    symbol: "XAUUSDT".into(),
                    qty: 1.25,
                    avg_price: 2348.7,
                    unrealized_pnl: 14.2,
                    realized_pnl: 6.4,
                },
            ],
            order_updates: vec![],
            recent_fills: vec![],
        })
        .await
        .context("failed to send user stream event")?;

    wait_until(Duration::from_secs(2), || {
        let runtime = app.snapshot().runtime;
        runtime.position_qty == 1.25
            && runtime.position_avg_price == 2348.7
            && runtime.unrealized_pnl == 14.2
            && runtime.realized_pnl == 6.4
    })
    .await?;
    wait_until(Duration::from_secs(2), || transport.keepalive_calls() > 0).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_binance_bootstrap_and_user_stream_sync_real_exchange_open_orders() -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_listen_key("listen-key-orders")
    .with_open_orders(vec![OpenOrder {
        order_id: "real_ord_01".into(),
        client_order_id: "real_grid_sell_01".into(),
        side: "sell".into(),
        price: 4510.25,
        qty: 0.2,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }]);
    let (_market_tx, market_rx) = mpsc::channel(16);
    let (user_tx, user_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;
    transport.push_user_stream(user_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.user_stream_keepalive_interval = Duration::from_millis(30);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_binance(config, Arc::new(transport.clone()));

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.execution.exchange_open_orders_source == OpenOrdersSource::ExchangeLive
            && snapshot.execution.exchange_open_orders.len() == 1
            && snapshot.execution.exchange_open_orders[0].order_id == "real_ord_01"
    })
    .await?;

    user_tx
        .send(UserStreamEvent {
            event_time_ms: now_ms + 2_000,
            positions: vec![],
            order_updates: vec![UserStreamOrderUpdate {
                symbol: "XAUUSDT".into(),
                order: OpenOrder {
                    order_id: "real_ord_01".into(),
                    client_order_id: "real_grid_sell_01".into(),
                    side: "sell".into(),
                    price: 4510.25,
                    qty: 0.2,
                    filled_qty: 0.2,
                    status: "FILLED".into(),
                    created_at: "2025-01-01T00:00:00Z".into(),
                    updated_at: "2025-01-01T00:00:02Z".into(),
                },
                is_terminal: true,
            }],
            recent_fills: vec![],
        })
        .await
        .context("failed to send order update event")?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.execution.exchange_open_orders_source == OpenOrdersSource::ExchangeLive
            && snapshot.execution.exchange_open_orders.is_empty()
    })
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_binance_user_stream_persists_passive_fill_for_recovery() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_listen_key("listen-key-passive-fill")
    .with_open_orders(vec![OpenOrder {
        order_id: "42".into(),
        client_order_id: "grid_buy_01".into(),
        side: "buy".into(),
        price: 4510.25,
        qty: 0.2,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }]);
    let (_market_tx, market_rx) = mpsc::channel(16);
    let (user_tx, user_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;
    transport.push_user_stream(user_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.user_stream_keepalive_interval = Duration::from_millis(30);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_sqlite_and_binance(
        &db_path,
        config,
        Arc::new(transport.clone()),
    )?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.user_stream_connected == Some(true)
            && snapshot.execution.exchange_open_orders_source == OpenOrdersSource::ExchangeLive
            && snapshot.execution.exchange_open_orders.len() == 1
    })
    .await?;

    user_tx
        .send(UserStreamEvent {
            event_time_ms: now_ms + 2_000,
            positions: vec![PositionSnapshot {
                symbol: "XAUUSDT".into(),
                qty: 0.1,
                avg_price: 4509.8,
                unrealized_pnl: 3.2,
                realized_pnl: 1.4,
            }],
            order_updates: vec![UserStreamOrderUpdate {
                symbol: "XAUUSDT".into(),
                order: OpenOrder {
                    order_id: "42".into(),
                    client_order_id: "grid_buy_01".into(),
                    side: "buy".into(),
                    price: 4510.25,
                    qty: 0.2,
                    filled_qty: 0.1,
                    status: "PARTIALLY_FILLED".into(),
                    created_at: "2025-01-01T00:00:00Z".into(),
                    updated_at: "2025-01-01T00:00:02Z".into(),
                },
                is_terminal: false,
            }],
            recent_fills: vec![UserStreamFill {
                symbol: "XAUUSDT".into(),
                fill: RecentFill {
                    trade_id: "binance_9001".into(),
                    order_id: "42".into(),
                    client_order_id: Some("grid_buy_01".into()),
                    side: "buy".into(),
                    price: 4509.8,
                    qty: 0.1,
                    fee: 0.001,
                    realized_pnl: 1.4,
                    event_time: "2025-01-01T00:00:02Z".into(),
                },
            }],
        })
        .await
        .context("failed to send passive fill user stream event")?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.runtime.position_qty == 0.1
            && snapshot.execution.recent_fills.len() == 1
            && snapshot.execution.recent_fills[0].trade_id == "binance_9001"
            && snapshot.execution.exchange_open_orders.len() == 1
            && snapshot.execution.exchange_open_orders[0].filled_qty == 0.1
    })
    .await
    .context("passive fill did not reach runtime snapshot")?;

    wait_until(Duration::from_secs(2), || {
        SqliteStorage::open(&db_path)
            .ok()
            .and_then(|storage| storage.load_runtime().ok().flatten())
            .is_some_and(|runtime| {
                runtime.snapshot.execution.recent_fills.len() == 1
                    && runtime.snapshot.execution.recent_fills[0].trade_id == "binance_9001"
                    && runtime.snapshot.runtime.position_qty == 0.1
            })
    })
    .await
    .context("passive fill did not persist to sqlite recovery snapshot")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_binance_user_stream_ignores_fill_for_other_symbol() -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_listen_key("listen-key-other-symbol");
    let (_market_tx, market_rx) = mpsc::channel(16);
    let (user_tx, user_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;
    transport.push_user_stream(user_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.user_stream_keepalive_interval = Duration::from_millis(30);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_binance(config, Arc::new(transport.clone()));

    wait_until(Duration::from_secs(2), || {
        app.snapshot().connection.user_stream_connected == Some(true)
    })
    .await?;

    user_tx
        .send(UserStreamEvent {
            event_time_ms: now_ms + 2_000,
            positions: vec![PositionSnapshot {
                symbol: "XAUUSDT".into(),
                qty: 1.25,
                avg_price: 2348.7,
                unrealized_pnl: 14.2,
                realized_pnl: 6.4,
            }],
            order_updates: vec![UserStreamOrderUpdate {
                symbol: "BTCUSDT".into(),
                order: OpenOrder {
                    order_id: "btc_ord_01".into(),
                    client_order_id: "grid_buy_01".into(),
                    side: "buy".into(),
                    price: 80_000.0,
                    qty: 0.1,
                    filled_qty: 0.1,
                    status: "FILLED".into(),
                    created_at: "2025-01-01T00:00:00Z".into(),
                    updated_at: "2025-01-01T00:00:02Z".into(),
                },
                is_terminal: true,
            }],
            recent_fills: vec![UserStreamFill {
                symbol: "BTCUSDT".into(),
                fill: RecentFill {
                    trade_id: "binance_9002".into(),
                    order_id: "btc_ord_01".into(),
                    client_order_id: Some("grid_buy_01".into()),
                    side: "buy".into(),
                    price: 80_000.0,
                    qty: 0.1,
                    fee: 0.001,
                    realized_pnl: 3.2,
                    event_time: "2025-01-01T00:00:02Z".into(),
                },
            }],
        })
        .await
        .context("failed to send cross-symbol user stream event")?;

    wait_until(Duration::from_secs(2), || {
        let runtime = app.snapshot().runtime;
        runtime.position_qty == 1.25
            && runtime.position_avg_price == 2348.7
            && runtime.unrealized_pnl == 14.2
            && runtime.realized_pnl == 6.4
    })
    .await?;

    let snapshot = app.snapshot();
    assert!(snapshot.execution.recent_fills.is_empty());
    assert!(snapshot.execution.exchange_open_orders.is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binance_immediate_fill_dedupes_rest_placeholder_when_user_stream_trade_arrives_later()
-> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_execution_enabled()
    .with_listen_key("listen-key-rest-first");
    let (_market_tx, market_rx) = mpsc::channel(16);
    let (user_tx, user_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;
    transport.push_user_stream(user_rx).await;

    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot.runtime.symbol = "XAUUSDT".into();
    runtime.snapshot.runtime.env = "testnet".into();
    runtime.snapshot.runtime.strategy_state = "paused".into();
    runtime.snapshot.runtime.position_qty = 0.1;
    runtime.snapshot.runtime.position_avg_price = 4500.0;
    runtime.snapshot.runtime.last_price = 4510.0;
    runtime.snapshot.runtime.mark_price = 4510.0;
    runtime.last_sequence = 1;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.user_stream_keepalive_interval = Duration::from_millis(30);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_runtime_and_binance(
        runtime,
        config,
        Arc::new(transport.clone()),
    );

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.http_available
            && snapshot.connection.user_stream_connected == Some(true)
    })
    .await?;

    let accepted = app
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_flatten_rest_first".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    wait_until(Duration::from_secs(2), || {
        app.snapshot()
            .execution
            .recent_fills
            .iter()
            .any(|fill| fill.trade_id == "binance_rest_order_cmd_flatten_rest_first_1735689600000")
    })
    .await
    .context("rest placeholder fill did not reach runtime snapshot")?;

    user_tx
        .send(UserStreamEvent {
            event_time_ms: now_ms + 2_000,
            positions: vec![],
            order_updates: vec![],
            recent_fills: vec![UserStreamFill {
                symbol: "XAUUSDT".into(),
                fill: RecentFill {
                    trade_id: "binance_9001".into(),
                    order_id: "order_cmd_flatten_rest_first".into(),
                    client_order_id: Some("reduce_only_cmd_flatten_rest_first".into()),
                    side: "sell".into(),
                    price: 4510.0,
                    qty: 0.1,
                    fee: 0.001,
                    realized_pnl: 1.0,
                    event_time: "2025-01-01T00:00:01Z".into(),
                },
            }],
        })
        .await
        .context("failed to send rest-first real fill event")?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.execution.recent_fills.len() == 1
            && snapshot.execution.recent_fills[0].trade_id == "binance_9001"
    })
    .await
    .context("real fill did not replace rest placeholder")?;

    assert!(
        app.snapshot()
            .execution
            .recent_fills
            .iter()
            .all(|fill| !fill.trade_id.starts_with("binance_rest_"))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binance_immediate_fill_dedupes_rest_placeholder_when_user_stream_trade_arrives_first()
-> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let submit_gate = Arc::new(Notify::new());
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_execution_enabled()
    .with_listen_key("listen-key-user-first")
    .with_reduce_only_submit_gate(submit_gate.clone());
    let (_market_tx, market_rx) = mpsc::channel(16);
    let (user_tx, user_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;
    transport.push_user_stream(user_rx).await;

    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot.runtime.symbol = "XAUUSDT".into();
    runtime.snapshot.runtime.env = "testnet".into();
    runtime.snapshot.runtime.strategy_state = "paused".into();
    runtime.snapshot.runtime.position_qty = 0.1;
    runtime.snapshot.runtime.position_avg_price = 4500.0;
    runtime.snapshot.runtime.last_price = 4510.0;
    runtime.snapshot.runtime.mark_price = 4510.0;
    runtime.last_sequence = 1;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.user_stream_keepalive_interval = Duration::from_millis(30);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_runtime_and_binance(
        runtime,
        config,
        Arc::new(transport.clone()),
    );

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.http_available
            && snapshot.connection.user_stream_connected == Some(true)
    })
    .await?;

    let accepted = app
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_flatten_user_first".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    wait_until(Duration::from_secs(2), || {
        transport.reduce_only_submit_blocked()
    })
    .await
    .context("reduce-only submit did not block before user stream fill")?;

    user_tx
        .send(UserStreamEvent {
            event_time_ms: now_ms + 2_000,
            positions: vec![],
            order_updates: vec![],
            recent_fills: vec![UserStreamFill {
                symbol: "XAUUSDT".into(),
                fill: RecentFill {
                    trade_id: "binance_9002".into(),
                    order_id: "order_cmd_flatten_user_first".into(),
                    client_order_id: Some("reduce_only_cmd_flatten_user_first".into()),
                    side: "sell".into(),
                    price: 4510.0,
                    qty: 0.1,
                    fee: 0.001,
                    realized_pnl: 1.0,
                    event_time: "2025-01-01T00:00:01Z".into(),
                },
            }],
        })
        .await
        .context("failed to send user-first real fill event")?;

    wait_until(Duration::from_millis(150), || {
        let snapshot = app.snapshot();
        snapshot.execution.recent_fills.len() == 1
            && snapshot.execution.recent_fills[0].trade_id == "binance_9002"
    })
    .await
    .context("real fill did not reach runtime snapshot before submit response")?;

    submit_gate.notify_waiters();

    wait_until(Duration::from_secs(2), || {
        app.snapshot()
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| {
                ack.command_id == "cmd_flatten_user_first" && ack.status == CommandStatus::Completed
            })
    })
    .await
    .context("flatten command did not complete after releasing delayed submit")?;

    let snapshot = app.snapshot();
    assert_eq!(snapshot.execution.recent_fills.len(), 1);
    assert_eq!(snapshot.execution.recent_fills[0].trade_id, "binance_9002");
    assert!(
        snapshot
            .execution
            .recent_fills
            .iter()
            .all(|fill| !fill.trade_id.starts_with("binance_rest_"))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binance_mode_routes_strategy_placements_through_transport_execution() -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_execution_enabled();
    let (market_tx, market_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_binance(config, Arc::new(transport.clone()));

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.http_available && snapshot.connection.ws_connected
    })
    .await
    .context("binance market bootstrap did not become ready")?;

    market_tx
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 1_000,
            last_price: Some(100.0),
            mark_price: Some(100.0),
        })
        .await
        .context("failed to send market event for strategy placement")?;

    wait_until(Duration::from_secs(2), || transport.submit_calls() == 6)
        .await
        .context("strategy placements did not route through transport execution")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binance_mode_routes_strategy_cancels_through_transport_without_absorbing_unrelated_orders()
-> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_execution_enabled();
    let (market_tx, market_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_binance(config, Arc::new(transport.clone()));

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.http_available && snapshot.connection.ws_connected
    })
    .await
    .context("binance market bootstrap did not become ready")?;

    market_tx
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 1_000,
            last_price: Some(100.0),
            mark_price: Some(100.0),
        })
        .await
        .context("failed to send market event for strategy placement")?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        transport.submit_calls() == 6 && snapshot.execution.open_orders.len() == 6
    })
    .await
    .context("strategy placements were not ready before cancel test")?;

    transport
        .set_cancel_result(vec![OpenOrder {
            order_id: "manual_ord_01".into(),
            client_order_id: "manual_order_01".into(),
            side: "buy".into(),
            price: 88.88,
            qty: 0.1,
            filled_qty: 0.0,
            status: "NEW".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        }])
        .await;

    let accepted = app
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_binance_cancel".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        transport.cancel_calls() == 1
            && snapshot.execution.open_orders.is_empty()
            && snapshot
                .execution
                .last_command_ack_event
                .as_ref()
                .is_some_and(|ack| {
                    ack.command_id == "cmd_binance_cancel" && ack.status == CommandStatus::Completed
                })
    })
    .await
    .context("cancel path did not finish through transport without polluting strategy mirror")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_clears_stale_strategy_mirror_so_resume_can_replace_real_binance_orders()
-> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;
    let mut persisted = PersistedRuntime::sqlite_bootstrap();
    persisted.snapshot.runtime.symbol = "XAUUSDT".into();
    persisted.snapshot.runtime.env = "testnet".into();
    persisted.snapshot.runtime.strategy_state = "paused".into();
    persisted.snapshot.runtime.last_price = 100.0;
    persisted.snapshot.runtime.mark_price = 100.0;
    persisted.snapshot.strategy.config = GridConfig {
        lower_price: 90.0,
        upper_price: 110.0,
        grid_levels: 6,
        max_position_notional: 3000.0,
        exchange_rules: None,
    };
    persisted.snapshot.execution.open_orders = vec![
        stale_strategy_order("buy", 1, 90.0),
        stale_strategy_order("buy", 2, 94.0),
        stale_strategy_order("buy", 3, 98.0),
        stale_strategy_order("sell", 4, 102.0),
        stale_strategy_order("sell", 5, 106.0),
        stale_strategy_order("sell", 6, 110.0),
    ];
    persisted.last_sequence = 1;
    storage.persist_runtime(&persisted)?;

    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
            order_rules: None,
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    )
    .with_execution_enabled()
    .with_open_orders(vec![]);
    let (_market_tx, market_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_startup_and_binance(
        &db_path,
        config,
        Arc::new(transport.clone()),
    )
    .await?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.http_available
            && snapshot.connection.ws_connected
            && snapshot.execution.open_orders.is_empty()
    })
    .await
    .context("binance startup did not clear stale strategy mirror before resume")?;

    let accepted = app
        .submit_command(
            CommandType::Resume,
            CommandRequest {
                command_id: "cmd_resume_after_reconcile".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    wait_until(Duration::from_secs(2), || transport.submit_calls() == 6)
        .await
        .context("resume did not replace missing strategy orders through transport execution")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binance_resume_aligns_strategy_orders_to_exchange_filters() -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "BTCUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "CRYPTO".into(),
            order_rules: Some(btc_testnet_order_rules()),
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: HashMap::new(),
        },
    )
    .with_execution_enabled();
    let (market_tx, market_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;

    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot.runtime.symbol = "BTCUSDT".into();
    runtime.snapshot.runtime.env = "testnet".into();
    runtime.snapshot.runtime.strategy_state = "paused".into();
    runtime.snapshot.strategy.config = GridConfig {
        lower_price: 65000.0,
        upper_price: 72000.0,
        grid_levels: 50,
        max_position_notional: 5000.0,
        exchange_rules: None,
    };
    let mut config = BinanceConfig::testnet("BTCUSDT");
    config.api_key = Some("demo-key".into());
    config.api_secret = Some("demo-secret".into());
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let app = Application::bootstrap_with_runtime_and_binance(
        runtime,
        config,
        Arc::new(transport.clone()),
    );

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.connection.http_available
            && snapshot.connection.ws_connected
            && snapshot.runtime.session_state == "continuous"
    })
    .await
    .context("binance runtime did not finish metadata bootstrap before resume")?;

    market_tx
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 1_000,
            last_price: Some(68594.3),
            mark_price: Some(68637.9),
        })
        .await
        .context("failed to send market event for BTC strategy placement")?;

    wait_until(Duration::from_secs(2), || {
        app.snapshot().runtime.mark_price > 0.0
    })
    .await
    .context("market price did not reach runtime before resume")?;

    let accepted = app
        .submit_command(
            CommandType::Resume,
            CommandRequest {
                command_id: "cmd_resume_btc_exchange_filters".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    wait_until(Duration::from_secs(2), || transport.submit_calls() == 50)
        .await
        .context("resume did not submit expected BTC strategy orders")?;

    let submit_requests = transport.submit_requests().await;
    assert_eq!(submit_requests.len(), 50);
    assert!(submit_requests.iter().all(|request| request.qty == 0.002));
    assert_eq!(
        submit_requests
            .iter()
            .find(|request| request.client_order_id == "grid_buy_02")
            .map(|request| request.price),
        Some(65142.9)
    );

    Ok(())
}

async fn wait_until<F>(within: Duration, predicate: F) -> Result<()>
where
    F: Fn() -> bool,
{
    let deadline = Instant::now() + within;
    loop {
        if predicate() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for expected condition"));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn btc_testnet_order_rules() -> ExchangeOrderRules {
    ExchangeOrderRules {
        price_tick: 0.1,
        price_precision: 2,
        min_price: 261.1,
        quantity_step: 0.001,
        quantity_precision: 3,
        min_qty: 0.001,
        min_notional: 100.0,
    }
}

fn stale_strategy_order(side: &str, step_id: u32, price: f64) -> OpenOrder {
    OpenOrder {
        order_id: format!("ord_{side}_{step_id:02}"),
        client_order_id: format!("grid_{side}_{step_id:02}"),
        side: side.into(),
        price,
        qty: 0.5,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }
}

fn now_ms_or_zero() -> i64 {
    Utc::now().timestamp_millis()
}
