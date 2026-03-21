use std::fs;
use std::time::Duration;
use std::{collections::VecDeque, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use grid_platform_service::{
    Application,
    integrations::binance::{
        BinanceConfig, BinanceTransport, ExchangeSymbol, MarketStreamEvent, TradingSchedule,
        TradingSession,
    },
    protocol::{
        CommandAck, CommandLinks, CommandRecord, CommandRequest, CommandStatus, CommandType,
        OpenOrdersSource, RuntimeSnapshot, SystemEvent,
    },
    storage::{PersistedRuntime, SqliteStorage},
};
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::Mutex;
use tokio::time::timeout;

#[derive(Clone)]
struct ScriptedBinanceTransport {
    exchange_info: ExchangeSymbol,
    trading_schedule: TradingSchedule,
    market_streams: Arc<Mutex<VecDeque<tokio::sync::mpsc::Receiver<MarketStreamEvent>>>>,
}

impl ScriptedBinanceTransport {
    fn new(exchange_info: ExchangeSymbol, trading_schedule: TradingSchedule) -> Self {
        Self {
            exchange_info,
            trading_schedule,
            market_streams: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    async fn push_market_stream(&self, receiver: tokio::sync::mpsc::Receiver<MarketStreamEvent>) {
        self.market_streams.lock().await.push_back(receiver);
    }
}

#[async_trait]
impl BinanceTransport for ScriptedBinanceTransport {
    async fn fetch_exchange_info(&self, symbol: &str) -> anyhow::Result<ExchangeSymbol> {
        if self.exchange_info.symbol != symbol {
            anyhow::bail!("unexpected symbol {symbol}");
        }
        Ok(self.exchange_info.clone())
    }

    async fn fetch_trading_schedule(&self) -> anyhow::Result<TradingSchedule> {
        Ok(self.trading_schedule.clone())
    }

    async fn connect_market_stream(
        &self,
        _symbol: &str,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<MarketStreamEvent>> {
        self.market_streams
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no scripted market stream available"))
    }

    async fn create_user_stream(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    async fn connect_user_stream(
        &self,
        _listen_key: &str,
    ) -> anyhow::Result<
        tokio::sync::mpsc::Receiver<grid_platform_service::integrations::binance::UserStreamEvent>,
    > {
        anyhow::bail!("user stream not expected in this test")
    }

    async fn keepalive_user_stream(&self, _listen_key: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn sqlite_storage_persists_command_audit_and_recovers_latest_runtime() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;

    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.runtime.strategy_state = "paused".into();
    snapshot.runtime.last_price = 2368.88;
    snapshot.execution.open_orders.truncate(1);
    snapshot.execution.last_command_ack = Some("cmd_pause_storage".into());
    snapshot.execution.last_command_ack_event = Some(CommandAck {
        command_id: "cmd_pause_storage".into(),
        command: CommandType::Pause,
        status: CommandStatus::Completed,
        message: "Strategy paused.".into(),
        links: CommandLinks::default(),
        emitted_at: "2025-01-01T00:01:00Z".into(),
    });
    snapshot.execution.recent_commands = vec![CommandRecord {
        command_id: "cmd_pause_storage".into(),
        command: CommandType::Pause,
        status: CommandStatus::Completed,
        summary: "Strategy paused.".into(),
        requested_at: "2025-01-01T00:00:58Z".into(),
        accepted_at: Some("2025-01-01T00:00:59Z".into()),
        finished_at: Some("2025-01-01T00:01:00Z".into()),
        links: CommandLinks::default(),
    }];

    storage.persist_runtime(&PersistedRuntime {
        snapshot,
        risk_events: vec![],
        system_events: vec![SystemEvent {
            level: "info".into(),
            source: "commands".into(),
            message: "Strategy paused.".into(),
            created_at: "2025-01-01T00:01:00Z".into(),
        }],
        last_sequence: 7,
    })?;

    let recovered = storage
        .load_runtime()?
        .expect("runtime should be persisted");
    let commands = storage.load_command_audit()?;

    assert_eq!(recovered.last_sequence, 7);
    assert_eq!(recovered.snapshot.runtime.strategy_state, "paused");
    assert_eq!(recovered.snapshot.runtime.last_price, 2368.88);
    assert_eq!(recovered.snapshot.execution.open_orders.len(), 1);
    assert_eq!(
        recovered.snapshot.execution.recent_commands[0].command_id,
        "cmd_pause_storage"
    );
    assert_eq!(recovered.system_events[0].source, "commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command_id, "cmd_pause_storage");

    Ok(())
}

#[test]
fn sqlite_storage_persists_failed_command_reason() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;

    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.execution.last_command_ack = Some("cmd_failed_storage".into());
    snapshot.execution.last_command_ack_event = Some(CommandAck {
        command_id: "cmd_failed_storage".into(),
        command: CommandType::CancelAll,
        status: CommandStatus::Failed,
        message: "exchange rejected cancel-all".into(),
        links: CommandLinks::default(),
        emitted_at: "2025-01-01T00:02:00Z".into(),
    });
    snapshot.execution.recent_commands = vec![CommandRecord {
        command_id: "cmd_failed_storage".into(),
        command: CommandType::CancelAll,
        status: CommandStatus::Failed,
        summary: "exchange rejected cancel-all".into(),
        requested_at: "2025-01-01T00:01:58Z".into(),
        accepted_at: Some("2025-01-01T00:01:59Z".into()),
        finished_at: Some("2025-01-01T00:02:00Z".into()),
        links: CommandLinks::default(),
    }];

    storage.persist_runtime(&PersistedRuntime {
        snapshot,
        risk_events: vec![],
        system_events: vec![SystemEvent {
            level: "error".into(),
            source: "commands".into(),
            message: "exchange rejected cancel-all".into(),
            created_at: "2025-01-01T00:02:00Z".into(),
        }],
        last_sequence: 9,
    })?;

    let recovered = storage
        .load_runtime()?
        .expect("runtime should be persisted");

    assert_eq!(
        recovered
            .snapshot
            .execution
            .recent_commands
            .first()
            .expect("recent command")
            .status,
        CommandStatus::Failed
    );
    assert_eq!(
        recovered
            .snapshot
            .execution
            .recent_commands
            .first()
            .expect("recent command")
            .summary,
        "exchange rejected cancel-all"
    );
    assert_eq!(
        recovered
            .snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .expect("ack event")
            .status,
        CommandStatus::Failed
    );

    Ok(())
}

#[test]
fn sqlite_storage_roundtrips_command_association_fields() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;

    let snapshot_json = json!({
        "connection": {
            "http_available": true,
            "ws_connected": false,
            "user_stream_connected": null,
            "latency_ms": 42,
            "last_heartbeat_at": "2025-01-01T00:00:00Z",
            "reconnect_backoff_ms": 0,
            "stale_age_ms": 0
        },
        "runtime": {
            "symbol": "XAUUSDT",
            "env": "testnet",
            "session_state": "regular",
            "strategy_state": "running",
            "last_price": 2361.48,
            "mark_price": 2361.55,
            "position_qty": 0.0,
            "position_avg_price": 2354.2,
            "unrealized_pnl": 0.0,
            "realized_pnl": 14.52
        },
        "execution": {
            "open_orders": [],
            "recent_fills": [{
                "trade_id": "fill_9001",
                "order_id": "ord_0999",
                "client_order_id": "flatten_reduce_only_01",
                "side": "buy",
                "price": 2349.1,
                "qty": 0.05,
                "fee": 0.03,
                "realized_pnl": 2.51,
                "event_time": "2025-01-01T00:00:00Z"
            }],
            "pending_commands": [],
            "last_command_ack": "cmd_flatten_01",
            "last_command_ack_event": {
                "command_id": "cmd_flatten_01",
                "command": "flatten_now",
                "status": "completed",
                "message": "Position flattened.",
                "client_order_ids": ["flatten_reduce_only_01"],
                "order_ids": ["ord_0999"],
                "trade_ids": ["fill_9001"],
                "emitted_at": "2025-01-01T00:00:05Z"
            },
            "recent_commands": [{
                "command_id": "cmd_flatten_01",
                "command": "flatten_now",
                "status": "completed",
                "summary": "Position flattened.",
                "requested_at": "2025-01-01T00:00:03Z",
                "accepted_at": "2025-01-01T00:00:04Z",
                "finished_at": "2025-01-01T00:00:05Z",
                "client_order_ids": ["flatten_reduce_only_01"],
                "order_ids": ["ord_0999"],
                "trade_ids": ["fill_9001"]
            }]
        },
        "risk": {
            "current_notional": 590.39,
            "max_notional": 1500.0,
            "daily_loss_limit": -120.0,
            "stop_loss_pct": 4.0,
            "risk_level": "watch",
            "breaker_engaged": false,
            "unacked_alerts": 1
        }
    });

    let snapshot: RuntimeSnapshot = serde_json::from_value(snapshot_json).expect("snapshot json");
    storage.persist_runtime(&PersistedRuntime {
        snapshot,
        risk_events: vec![],
        system_events: vec![],
        last_sequence: 10,
    })?;

    let recovered = storage
        .load_runtime()?
        .expect("runtime should be persisted");
    let serialized = serde_json::to_value(&recovered.snapshot).expect("serialize recovered");

    assert_eq!(
        serialized["execution"]["recent_fills"][0]["client_order_id"],
        "flatten_reduce_only_01"
    );
    assert_eq!(
        serialized["execution"]["last_command_ack_event"]["order_ids"][0],
        "ord_0999"
    );
    assert_eq!(
        serialized["execution"]["recent_commands"][0]["trade_ids"][0],
        "fill_9001"
    );
    assert_eq!(
        serialized["execution"]["exchange_open_orders_source"],
        "unavailable"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_binance_bootstrap_normalizes_open_orders_source_on_recovery() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;

    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.runtime.symbol = "XAUUSDT".into();
    snapshot.runtime.env = "testnet".into();
    snapshot.execution.open_orders_source = OpenOrdersSource::ExchangeLive;
    storage.persist_runtime(&PersistedRuntime {
        snapshot,
        risk_events: vec![],
        system_events: vec![],
        last_sequence: 1,
    })?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let transport = ScriptedBinanceTransport::new(
        ExchangeSymbol {
            symbol: "XAUUSDT".into(),
            status: "TRADING".into(),
            underlying_type: "COMMODITY".into(),
        },
        TradingSchedule {
            update_time_ms: now_ms,
            market_schedules: std::collections::HashMap::from([(
                "COMMODITY".into(),
                vec![TradingSession {
                    start_time_ms: now_ms - 60_000,
                    end_time_ms: now_ms + 60_000,
                    session_type: "REGULAR".into(),
                }],
            )]),
        },
    );
    let (market_tx, market_rx) = tokio::sync::mpsc::channel(1);
    transport.push_market_stream(market_rx).await;

    let mut config = BinanceConfig::testnet("XAUUSDT");
    config.metadata_refresh_interval = Duration::from_secs(3600);
    config.health_tick_interval = Duration::from_secs(3600);
    config.reconnect_base_delay = Duration::from_secs(3600);
    config.reconnect_max_delay = Duration::from_secs(3600);

    let app =
        Application::bootstrap_with_sqlite_and_binance(&db_path, config, Arc::new(transport))?;
    drop(market_tx);

    assert_eq!(
        app.snapshot().execution.open_orders_source,
        OpenOrdersSource::StrategyMirror
    );

    Ok(())
}

#[test]
fn sqlite_storage_recovers_open_orders_source_from_legacy_snapshot() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;

    let snapshot_json = json!({
        "connection": {
            "http_available": true,
            "ws_connected": false,
            "user_stream_connected": null,
            "latency_ms": 42,
            "last_heartbeat_at": "2025-01-01T00:00:00Z",
            "reconnect_backoff_ms": 0,
            "stale_age_ms": 0
        },
        "runtime": {
            "symbol": "XAUUSDT",
            "env": "testnet",
            "session_state": "regular",
            "strategy_state": "running",
            "last_price": 2361.48,
            "mark_price": 2361.55,
            "position_qty": 0.0,
            "position_avg_price": 2354.2,
            "unrealized_pnl": 0.0,
            "realized_pnl": 14.52
        },
        "execution": {
            "open_orders": [],
            "recent_fills": [],
            "pending_commands": [],
            "last_command_ack": null,
            "last_command_ack_event": null,
            "recent_commands": []
        },
        "risk": {
            "current_notional": 590.39,
            "max_notional": 1500.0,
            "daily_loss_limit": -120.0,
            "stop_loss_pct": 4.0,
            "risk_level": "watch",
            "breaker_engaged": false,
            "unacked_alerts": 1
        }
    });

    let snapshot: RuntimeSnapshot = serde_json::from_value(snapshot_json).expect("snapshot json");
    storage.persist_runtime(&PersistedRuntime {
        snapshot,
        risk_events: vec![],
        system_events: vec![],
        last_sequence: 10,
    })?;

    let recovered = storage
        .load_runtime()?
        .expect("runtime should be persisted");
    let serialized = serde_json::to_value(&recovered.snapshot).expect("serialize recovered");

    assert_eq!(
        serialized["execution"]["open_orders_source"],
        "strategy_mirror"
    );
    assert_eq!(
        serialized["execution"]["exchange_open_orders_source"],
        "unavailable"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_bootstrap_uses_cold_start_message_for_empty_database() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");

    let application = Application::bootstrap_with_sqlite(&db_path)?;

    assert_eq!(application.system_events()[0].source, "bootstrap");
    assert_eq!(
        application.system_events()[0].message,
        "Rust runtime bootstrapped with SQLite storage."
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn application_restart_recovers_latest_runtime_snapshot_from_sqlite() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");

    let application = Application::bootstrap_with_sqlite(&db_path)?;
    application
        .submit_command(
            CommandType::Pause,
            CommandRequest {
                command_id: "cmd_pause_restart".into(),
            },
        )
        .await?;
    let tick = application.emit_price_tick().await?;
    let snapshot_before_restart = application.snapshot();
    drop(application);

    let recovered = Application::bootstrap_with_sqlite(&db_path)?;
    let snapshot_after_restart = recovered.snapshot();

    assert_eq!(snapshot_after_restart.runtime.strategy_state, "paused");
    assert_eq!(snapshot_after_restart.runtime.last_price, tick.last_price);
    assert_eq!(
        snapshot_after_restart.runtime.last_price,
        snapshot_before_restart.runtime.last_price
    );
    assert!(
        snapshot_after_restart
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| ack.command_id == "cmd_pause_restart")
    );
    assert_eq!(
        snapshot_after_restart.execution.recent_commands[0].command_id,
        "cmd_pause_restart"
    );
    assert_eq!(recovered.system_events()[0].source, "commands");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_storage_keeps_full_command_audit_beyond_recent_command_window() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let application = Application::bootstrap_with_sqlite(&db_path)?;

    for index in 0..30 {
        let command = if index % 2 == 0 {
            CommandType::Pause
        } else {
            CommandType::Resume
        };
        application
            .submit_command(
                command,
                CommandRequest {
                    command_id: format!("cmd_audit_{index:02}"),
                },
            )
            .await?;
    }
    drop(application);

    let storage = SqliteStorage::open(&db_path)?;
    let recovered = storage
        .load_runtime()?
        .expect("runtime should be persisted");
    let audit = storage.load_command_audit()?;

    assert_eq!(audit.len(), 30);
    assert_eq!(audit[0].command_id, "cmd_audit_29");
    assert_eq!(
        audit.last().expect("oldest command").command_id,
        "cmd_audit_00"
    );
    assert_eq!(recovered.snapshot.execution.recent_commands.len(), 24);
    assert_eq!(
        recovered.snapshot.execution.recent_commands[0].command_id,
        "cmd_audit_29"
    );
    assert_eq!(
        recovered
            .snapshot
            .execution
            .recent_commands
            .last()
            .expect("recent command window tail")
            .command_id,
        "cmd_audit_06"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_idempotent_hit_uses_command_audit_beyond_recent_window() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let application = Application::bootstrap_with_sqlite(&db_path)?;

    for index in 0..30 {
        let command = if index % 2 == 0 {
            CommandType::Pause
        } else {
            CommandType::Resume
        };
        application
            .submit_command(
                command,
                CommandRequest {
                    command_id: format!("cmd_audit_{index:02}"),
                },
            )
            .await?;
    }

    assert_eq!(application.snapshot().runtime.strategy_state, "running");

    application
        .submit_command(
            CommandType::Pause,
            CommandRequest {
                command_id: "cmd_audit_00".into(),
            },
        )
        .await?;

    let snapshot = application.snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "running");
    assert!(
        snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| ack.message.contains("Idempotent hit"))
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_rejects_reused_command_id_with_different_command_type() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let application = Application::bootstrap_with_sqlite(&db_path)?;

    for index in 0..30 {
        let command = if index % 2 == 0 {
            CommandType::Pause
        } else {
            CommandType::Resume
        };
        application
            .submit_command(
                command,
                CommandRequest {
                    command_id: format!("cmd_audit_{index:02}"),
                },
            )
            .await?;
    }

    let before = application.snapshot();
    let error = application
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_audit_00".into(),
            },
        )
        .await
        .expect_err("mismatched command_id reuse should be rejected");
    assert!(error.to_string().contains("different command"));

    let after = application.snapshot();
    assert_eq!(
        after.execution.last_command_ack_event,
        before.execution.last_command_ack_event
    );
    assert_eq!(
        after.execution.recent_commands[0].command,
        CommandType::Resume
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_persist_failure_rejects_command_and_rolls_back_runtime() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");

    let application = Application::bootstrap_with_sqlite(&db_path)?;
    let mut events_rx = application.subscribe_events();

    fs::remove_file(&db_path)?;
    fs::create_dir(&db_path)?;

    let error = application
        .submit_command(
            CommandType::Pause,
            CommandRequest {
                command_id: "cmd_pause_persist_fail".into(),
            },
        )
        .await
        .expect_err("command should fail when sqlite persistence fails");
    assert!(error.to_string().contains("failed to open sqlite db"));

    let snapshot = application.snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "running");
    assert!(snapshot.execution.last_command_ack_event.is_none());
    assert!(snapshot.execution.recent_commands.is_empty());

    assert!(
        timeout(Duration::from_millis(200), events_rx.recv())
            .await
            .is_err()
    );

    Ok(())
}
