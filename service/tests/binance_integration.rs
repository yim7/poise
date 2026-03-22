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
    integrations::binance::{
        BinanceConfig, BinanceTransport, ExchangeSymbol, MarketStreamEvent, PositionSnapshot,
        TradingSchedule, TradingSession, UserStreamEvent, UserStreamOrderUpdate,
    },
    protocol::{OpenOrder, OpenOrdersSource, RuntimeSnapshot, SystemEvent},
    storage::{PersistedRuntime, SqliteStorage},
};
use tempfile::tempdir;
use tokio::{
    sync::{Mutex, mpsc},
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
    listen_key: Option<String>,
    open_orders: Option<Vec<OpenOrder>>,
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
            listen_key: None,
            open_orders: None,
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

    fn market_connects(&self) -> usize {
        self.market_connects.load(Ordering::SeqCst)
    }

    fn keepalive_calls(&self) -> usize {
        self.keepalive_calls.load(Ordering::SeqCst)
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_binance_market_sync_updates_session_prices_stale_and_reconnects() -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = FakeBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
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

fn now_ms_or_zero() -> i64 {
    Utc::now().timestamp_millis()
}
