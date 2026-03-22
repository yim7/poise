use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use grid_platform_service::{
    Application,
    integrations::binance::{
        BinanceTransport, ExchangeSymbol, MarketStreamEvent, PositionSnapshot,
        PositionSnapshotState, TradingSchedule, UserStreamEvent,
    },
    protocol::{OpenOrder, OpenOrdersSource},
    startup::{
        RuntimeMode, StartupDecision, StartupExchangeState, StartupReport,
        collect_startup_exchange_state, reconcile_startup,
    },
    storage::PersistedRuntime,
};
use tempfile::{TempDir, tempdir};
use tokio::sync::mpsc;

#[derive(Clone)]
struct FakeBinanceTransport {
    exchange_info: ExchangeSymbol,
    trading_schedule: TradingSchedule,
    position: PositionSnapshotState,
    open_orders: Option<Vec<OpenOrder>>,
}

impl FakeBinanceTransport {
    fn new() -> Self {
        Self {
            exchange_info: ExchangeSymbol {
                symbol: "XAUUSDT".into(),
                status: "TRADING".into(),
                underlying_type: "COMMODITY".into(),
            },
            trading_schedule: TradingSchedule {
                update_time_ms: 0,
                market_schedules: std::collections::HashMap::new(),
            },
            position: PositionSnapshotState::Flat,
            open_orders: Some(Vec::new()),
        }
    }

    fn with_position_snapshot(mut self, position: PositionSnapshot) -> Self {
        self.position = PositionSnapshotState::Position(position);
        self
    }

    fn with_open_orders(mut self, open_orders: Vec<OpenOrder>) -> Self {
        self.open_orders = Some(open_orders);
        self
    }

    fn with_unavailable_signed_state(mut self) -> Self {
        self.position = PositionSnapshotState::Unavailable;
        self.open_orders = None;
        self
    }
}

#[async_trait]
impl BinanceTransport for FakeBinanceTransport {
    async fn fetch_exchange_info(&self, symbol: &str) -> Result<ExchangeSymbol> {
        if self.exchange_info.symbol != symbol {
            return Err(anyhow!("unexpected symbol {symbol}"));
        }
        Ok(self.exchange_info.clone())
    }

    async fn fetch_trading_schedule(&self) -> Result<TradingSchedule> {
        Ok(self.trading_schedule.clone())
    }

    async fn fetch_position_snapshot(&self, _symbol: &str) -> Result<PositionSnapshotState> {
        Ok(self.position.clone())
    }

    async fn connect_market_stream(
        &self,
        _symbol: &str,
    ) -> Result<mpsc::Receiver<MarketStreamEvent>> {
        Err(anyhow!("not used in startup preflight test"))
    }

    async fn create_user_stream(&self) -> Result<Option<String>> {
        Err(anyhow!("not used in startup preflight test"))
    }

    async fn connect_user_stream(
        &self,
        _listen_key: &str,
    ) -> Result<mpsc::Receiver<UserStreamEvent>> {
        Err(anyhow!("not used in startup preflight test"))
    }

    async fn keepalive_user_stream(&self, _listen_key: &str) -> Result<()> {
        Err(anyhow!("not used in startup preflight test"))
    }

    async fn fetch_open_orders(&self, _symbol: &str) -> Result<Option<Vec<OpenOrder>>> {
        Ok(self.open_orders.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_preflight_collects_exchange_position_and_open_orders() -> Result<()> {
    let transport = FakeBinanceTransport::new()
        .with_position_snapshot(PositionSnapshot {
            symbol: "XAUUSDT".into(),
            qty: 1.25,
            avg_price: 2368.5,
            unrealized_pnl: 0.0,
            realized_pnl: 0.0,
        })
        .with_open_orders(vec![sample_open_order("grid_buy_01")]);

    let state = collect_startup_exchange_state("XAUUSDT", Arc::new(transport)).await?;

    assert_eq!(
        state.position_snapshot().expect("position snapshot").qty,
        1.25
    );
    assert_eq!(state.open_orders.as_ref().expect("open orders").len(), 1);
    Ok(())
}

#[test]
fn startup_reconcile_pauses_when_exchange_position_exists_but_persisted_runtime_is_flat() {
    let persisted = PersistedRuntime::sqlite_bootstrap();
    let exchange = StartupExchangeState {
        position: PositionSnapshotState::Position(PositionSnapshot {
            symbol: "XAUUSDT".into(),
            qty: 1.0,
            avg_price: 2360.0,
            unrealized_pnl: 0.0,
            realized_pnl: 0.0,
        }),
        open_orders: Some(vec![]),
    };

    let decision =
        reconcile_startup(RuntimeMode::Mainnet, &persisted, &exchange).expect("decision");

    assert!(matches!(
        decision,
        StartupDecision::Pause { code, .. }
            if code == "STARTUP_RECONCILE_POSITION_MISMATCH"
    ));
}

#[test]
fn startup_reconcile_pauses_when_persisted_runtime_has_position_but_exchange_is_flat() {
    let mut persisted = PersistedRuntime::sqlite_bootstrap();
    persisted.snapshot.runtime.position_qty = 1.0;
    persisted.snapshot.runtime.position_avg_price = 2360.0;

    let exchange = StartupExchangeState {
        position: PositionSnapshotState::Flat,
        open_orders: Some(vec![]),
    };

    let decision =
        reconcile_startup(RuntimeMode::Mainnet, &persisted, &exchange).expect("decision");

    assert!(matches!(
        decision,
        StartupDecision::Pause { code, .. }
            if code == "STARTUP_RECONCILE_POSITION_MISMATCH"
    ));
}

#[test]
fn startup_reconcile_pauses_when_exchange_open_orders_differ_from_persisted_state() {
    let mut persisted = PersistedRuntime::sqlite_bootstrap();
    persisted.snapshot.execution.exchange_open_orders = vec![sample_open_order("grid_buy_01")];
    persisted.snapshot.execution.exchange_open_orders_source = OpenOrdersSource::ExchangeLive;

    let exchange = StartupExchangeState {
        position: PositionSnapshotState::Flat,
        open_orders: Some(vec![sample_open_order("grid_buy_02")]),
    };

    let decision =
        reconcile_startup(RuntimeMode::Mainnet, &persisted, &exchange).expect("decision");

    assert!(matches!(
        decision,
        StartupDecision::Pause { code, .. }
            if code == "STARTUP_RECONCILE_OPEN_ORDERS_MISMATCH"
    ));
}

#[test]
fn startup_apply_clears_stale_strategy_mirror_when_exchange_has_no_live_orders() {
    let mut persisted = PersistedRuntime::sqlite_bootstrap();
    persisted.snapshot.execution.open_orders = vec![OpenOrder {
        order_id: "ord_buy_01".into(),
        client_order_id: "grid_buy_01".into(),
        side: "BUY".into(),
        price: 2360.0,
        qty: 0.1,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }];
    persisted.snapshot.execution.open_orders_source = OpenOrdersSource::StrategyMirror;

    let applied = StartupReport {
        exchange: StartupExchangeState {
            position: PositionSnapshotState::Flat,
            open_orders: Some(vec![]),
        },
        decision: StartupDecision::Continue,
    }
    .apply_to(persisted);

    assert!(
        applied.snapshot.execution.open_orders.is_empty(),
        "startup should not retain stale local strategy mirror orders when exchange is empty"
    );
    assert_eq!(
        applied.snapshot.execution.exchange_open_orders_source,
        OpenOrdersSource::ExchangeLive
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_refuses_mainnet_when_signed_exchange_state_is_unavailable() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let transport = Arc::new(FakeBinanceTransport::new().with_unavailable_signed_state());

    let error = Application::bootstrap_with_startup_and_binance(
        &db_path,
        grid_platform_service::integrations::binance::BinanceConfig::mainnet("XAUUSDT"),
        transport,
    )
    .await
    .err()
    .expect("mainnet should refuse unavailable signed startup state");

    assert!(
        error
            .to_string()
            .contains("STARTUP_MAINNET_SIGNED_STATE_UNAVAILABLE")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_applies_pause_decision_before_supervisor_starts() -> Result<()> {
    let (_temp, app) = build_mainnet_application_with_reconcile_pause().await?;
    let snapshot = app.snapshot();

    assert_eq!(snapshot.runtime.strategy_state, "paused");
    assert!(
        app.system_events()
            .iter()
            .any(|event| event.code.as_deref() == Some("STARTUP_RECONCILE_POSITION_MISMATCH"))
    );
    Ok(())
}

fn sample_open_order(client_order_id: &str) -> OpenOrder {
    OpenOrder {
        order_id: format!("order-{client_order_id}"),
        client_order_id: client_order_id.into(),
        side: "BUY".into(),
        price: 2360.0,
        qty: 0.1,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }
}

async fn build_mainnet_application_with_reconcile_pause() -> Result<(TempDir, Application)> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let transport = Arc::new(FakeBinanceTransport::new().with_position_snapshot(
        PositionSnapshot {
            symbol: "XAUUSDT".into(),
            qty: 1.0,
            avg_price: 2360.0,
            unrealized_pnl: 0.0,
            realized_pnl: 0.0,
        },
    ));

    let app = Application::bootstrap_with_startup_and_binance(
        &db_path,
        grid_platform_service::integrations::binance::BinanceConfig::mainnet("XAUUSDT"),
        transport,
    )
    .await?;

    Ok((temp, app))
}
