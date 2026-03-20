use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use grid_platform_service::{
    Application,
    integrations::binance::{
        BinanceConfig, BinanceTransport, ExchangeSymbol, MarketStreamEvent, TradingSchedule,
        TradingSession, UserStreamEvent,
    },
    protocol::{OpenOrder, RuntimeSnapshot},
    storage::PersistedRuntime,
};
use tokio::{
    sync::{Mutex, mpsc},
    time::sleep,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_transport_market_event_drives_paper_fill_into_service_snapshot() -> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = ScriptedBinanceTransport::new(
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
    let (market_tx, market_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.last_sequence = 1;
    runtime.snapshot = paused_snapshot_with_buy_order();
    runtime.snapshot.runtime.env = "testnet".into();

    let app = Application::bootstrap_with_runtime_and_binance(
        runtime,
        config,
        Arc::new(transport.clone()),
    );

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.runtime.session_state == "regular" && snapshot.connection.http_available
    })
    .await
    .context("service did not finish fake binance bootstrap")?;

    market_tx
        .send(MarketStreamEvent {
            event_time_ms: now_ms + 1_000,
            last_price: Some(99.5),
            mark_price: Some(99.5),
        })
        .await
        .context("failed to send scripted market event")?;

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.execution.open_orders.is_empty()
            && snapshot.execution.recent_fills.iter().any(|fill| {
                fill.client_order_id.as_deref() == Some("grid_buy_01")
                    && fill.order_id == "ord_buy_01"
            })
            && snapshot.runtime.position_qty == 1.0
    })
    .await
    .context("paper fill did not appear in service runtime snapshot")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_transport_bootstrap_waits_for_first_market_price_before_placing_grid_orders()
-> Result<()> {
    let now_ms = Utc::now().timestamp_millis();
    let transport = ScriptedBinanceTransport::new(
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
    let (_market_tx, market_rx) = mpsc::channel(16);
    transport.push_market_stream(market_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.metadata_refresh_interval = Duration::from_millis(250);
    config.health_tick_interval = Duration::from_millis(25);
    config.reconnect_base_delay = Duration::from_millis(40);
    config.reconnect_max_delay = Duration::from_millis(80);

    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.last_sequence = 1;
    runtime.snapshot.runtime.env = "testnet".into();
    runtime.snapshot.runtime.strategy_state = "running".into();
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

    let app = Application::bootstrap_with_runtime_and_binance(
        runtime,
        config,
        Arc::new(transport.clone()),
    );

    wait_until(Duration::from_secs(2), || {
        let snapshot = app.snapshot();
        snapshot.runtime.session_state == "regular" && snapshot.connection.http_available
    })
    .await
    .context("service did not finish fake binance bootstrap")?;

    let snapshot = app.snapshot();
    assert!(
        snapshot.execution.open_orders.is_empty(),
        "grid orders should wait for the first valid market price"
    );
    assert!(
        snapshot.execution.recent_fills.is_empty(),
        "paper fills should not appear before the first valid market price"
    );

    Ok(())
}

#[derive(Clone)]
struct ScriptedBinanceTransport {
    exchange_info: ExchangeSymbol,
    trading_schedule: TradingSchedule,
    market_streams: Arc<Mutex<VecDeque<mpsc::Receiver<MarketStreamEvent>>>>,
}

impl ScriptedBinanceTransport {
    fn new(exchange_info: ExchangeSymbol, trading_schedule: TradingSchedule) -> Self {
        Self {
            exchange_info,
            trading_schedule,
            market_streams: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    async fn push_market_stream(&self, receiver: mpsc::Receiver<MarketStreamEvent>) {
        self.market_streams.lock().await.push_back(receiver);
    }
}

#[async_trait]
impl BinanceTransport for ScriptedBinanceTransport {
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
        anyhow::bail!("user stream is not used in this test")
    }

    async fn keepalive_user_stream(&self, _listen_key: &str) -> Result<()> {
        Ok(())
    }
}

fn paused_snapshot_with_buy_order() -> RuntimeSnapshot {
    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.runtime.strategy_state = "paused".into();
    snapshot.runtime.last_price = 100.0;
    snapshot.runtime.mark_price = 100.0;
    snapshot.runtime.position_qty = 0.0;
    snapshot.runtime.position_avg_price = 0.0;
    snapshot.runtime.unrealized_pnl = 0.0;
    snapshot.runtime.realized_pnl = 0.0;
    snapshot.execution.open_orders = vec![OpenOrder {
        order_id: "ord_buy_01".into(),
        client_order_id: "grid_buy_01".into(),
        side: "buy".into(),
        price: 100.0,
        qty: 1.0,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }];
    snapshot.execution.recent_fills.clear();
    snapshot.execution.pending_commands.clear();
    snapshot.execution.last_command_ack = None;
    snapshot.execution.last_command_ack_event = None;
    snapshot.execution.recent_commands.clear();
    snapshot
}

async fn wait_until<F>(within: Duration, predicate: F) -> Result<()>
where
    F: Fn() -> bool,
{
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return Ok(());
        }
        sleep(Duration::from_millis(25)).await;
    }
    Err(anyhow!("timed out waiting for expected test state"))
}
