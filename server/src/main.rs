mod assembly;
mod config;
mod effect_worker;
mod http;
mod notifications;
mod order_outcome;
#[allow(dead_code)]
mod projector;
#[allow(dead_code)]
mod query_service;
#[allow(dead_code)]
mod read_model;
mod runtime;
mod state_bootstrap;
mod websocket;
mod write_service;

use std::env;

use anyhow::Result;
use state_bootstrap::{StateBootstrapError, StateBootstrapMode, SuggestedAction};

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupOptions {
    config_path: String,
    bootstrap_mode: StateBootstrapMode,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Poise server starting");

    let options = parse_startup_options(env::args().skip(1))?;
    let config = config::load_config(&options.config_path)?;
    let repository = match state_bootstrap::prepare_state_repository(&config, options.bootstrap_mode).await {
        Ok(repository) => repository,
        Err(StateBootstrapError::Unexpected(error)) => return Err(error),
        Err(error) => return Err(anyhow::anyhow!(render_startup_error(&error))),
    };
    let platform = assembly::assemble(&config, repository).await?;
    let runtime_handles = platform.runtime.start().await?;

    let app = http::router(platform.state());
    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    platform.runtime.shutdown(runtime_handles).await;
    serve_result?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl+c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn parse_startup_options(mut args: impl Iterator<Item = String>) -> Result<StartupOptions> {
    let mut config_path = None;
    let mut bootstrap_mode = StateBootstrapMode::Strict;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --config"))?;
                config_path = Some(value);
            }
            "--rebuild-state" => {
                bootstrap_mode = StateBootstrapMode::Rebuild;
            }
            other => {
                return Err(anyhow::anyhow!("unknown argument: {other}"));
            }
        }
    }

    Ok(StartupOptions {
        config_path: config_path
            .ok_or_else(|| anyhow::anyhow!("missing required --config <path>"))?,
        bootstrap_mode,
    })
}

fn render_startup_error(error: &StateBootstrapError) -> String {
    match error {
        StateBootstrapError::PersistedStateMismatch {
            db_path,
            mismatches,
            suggested_action,
        } => {
            let mut rendered = format!(
                "persisted state does not match current config in `{}`.",
                db_path.display()
            );

            for mismatch in mismatches {
                let instrument_line =
                    if mismatch.expected_instrument != mismatch.actual_instrument {
                        format!(
                            "\n  instrument: expected `{}:{}`, persisted `{}:{}`",
                            mismatch.expected_instrument.venue.as_str(),
                            mismatch.expected_instrument.symbol,
                            mismatch.actual_instrument.venue.as_str(),
                            mismatch.actual_instrument.symbol
                        )
                    } else {
                        String::new()
                    };

                rendered.push_str(&format!(
                    "\ntrack `{}`:{}\n  expected config: {}\n  persisted config: {}",
                    mismatch.track_id,
                    instrument_line,
                    mismatch.expected_config_json,
                    mismatch.actual_config_json
                ));
            }

            match suggested_action {
                SuggestedAction::RebuildState => {
                    rendered.push_str(
                        "\nuse `--rebuild-state` to back up the old database, discard local snapshots, and rebuild state from the exchange's live positions and orders.\n\
suggested command: cargo run -p poise-server -- --config <path> --rebuild-state",
                    );
                }
            }

            rendered
        }
        StateBootstrapError::Unexpected(error) => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use chrono::Utc;
    use poise_core::types::ExchangeRules;
    use poise_engine::ports::{
        ClockPort, ExchangeInfo, ExchangeOrder, ExchangePort, OrderReceipt, OrderRequest,
        OrderStatus, Position, PriceTick,
    };
    use poise_engine::track::{Instrument, Venue};
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use crate::state_bootstrap::{StateBootstrapError, StateBootstrapMode, SuggestedAction};

    use super::{StartupOptions, parse_startup_options};

    fn unique_test_environment() -> String {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        format!(
            "main-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn parse_config_path_requires_config_flag() {
        let error = parse_startup_options(Vec::<String>::new().into_iter()).unwrap_err();
        assert!(error.to_string().contains("--config"));
    }

    #[test]
    fn parse_config_path_reads_flag_value() {
        let options = parse_startup_options(
            vec!["--config".to_string(), "configs/test.demo.toml".to_string()].into_iter(),
        )
        .unwrap();
        assert_eq!(
            options,
            StartupOptions {
                config_path: "configs/test.demo.toml".to_string(),
                bootstrap_mode: StateBootstrapMode::Strict,
            }
        );
    }

    #[test]
    fn parse_config_path_accepts_rebuild_state_flag() {
        let options = parse_startup_options(
            vec![
                "--rebuild-state".to_string(),
                "--config".to_string(),
                "configs/test.demo.toml".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();

        assert_eq!(
            options,
            StartupOptions {
                config_path: "configs/test.demo.toml".to_string(),
                bootstrap_mode: StateBootstrapMode::Rebuild,
            }
        );
    }

    #[test]
    fn parse_config_path_rejects_unknown_arguments() {
        let error = parse_startup_options(vec!["--bogus".to_string()].into_iter()).unwrap_err();
        assert!(error.to_string().contains("unknown argument"));
    }

    #[test]
    fn render_startup_error_formats_structured_mismatch_for_cli() {
        let rendered = super::render_startup_error(&StateBootstrapError::PersistedStateMismatch {
            db_path: std::path::PathBuf::from(".data/testnet/poise-server.sqlite"),
            mismatches: vec![crate::state_bootstrap::PersistedStateMismatch {
                track_id: "btc-core".into(),
                expected_instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                actual_instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                expected_config_json: r#"{"lower_price":90.0}"#.into(),
                actual_config_json: r#"{"lower_price":80.0}"#.into(),
            }],
            suggested_action: SuggestedAction::RebuildState,
        });

        assert!(rendered.contains(".data/testnet/poise-server.sqlite"));
        assert!(rendered.contains("btc-core"));
        assert!(rendered.contains("--rebuild-state"));
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn probe_health_script_path() -> PathBuf {
        workspace_root().join("scripts").join("probe-health.sh")
    }

    fn run_paper_tui_script_path() -> PathBuf {
        workspace_root().join("scripts").join("run-paper-tui.sh")
    }

    fn paper_layout_path() -> PathBuf {
        workspace_root()
            .join("ops")
            .join("zellij")
            .join("poise-paper.kdl")
    }

    #[test]
    fn paper_layout_prefers_tui_in_primary_left_pane() {
        let layout = fs::read_to_string(paper_layout_path()).unwrap();

        assert!(layout.contains("pane size=\"72%\" command=\"bash\""));
        assert!(layout.contains("./scripts/run-paper-tui.sh"));
        assert!(layout.contains("pane size=\"68%\" command=\"bash\""));
        assert!(layout.contains("./scripts/run-paper-server.sh"));
        assert!(layout.contains("pane size=\"32%\" command=\"bash\""));
        assert!(layout.contains("./scripts/probe-health.sh"));
    }

    #[test]
    fn run_paper_tui_script_supports_dry_run() {
        let output = Command::new("bash")
            .arg(run_paper_tui_script_path())
            .arg("--dry-run")
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("base_url="));
        assert!(stdout.contains("command=cargo run -p poise-tui"));
    }

    async fn wait_for_child_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
        for _ in 0..80 {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let _ = child.kill();
        let _ = child.wait();
        panic!("child process did not exit within timeout");
    }

    #[tokio::test]
    async fn probe_health_exits_after_failure_threshold_and_runs_alert_hook() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind_address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/health",
            get(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    r#"{"status":"attention_required","track_count":1,"attention_required_count":1}"#,
                )
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let temp_dir = tempfile::tempdir().unwrap();
        let log_dir = temp_dir.path().join("logs");
        let hook_output_path = temp_dir.path().join("hook.log");
        let hook_command = format!(
            "printf 'alert:%s:%s\\n' \"$POISE_HEALTH_FAILURE_COUNT\" \"$POISE_HEALTH_LAST_STATUS\" >> {}",
            hook_output_path.display()
        );

        let mut child = Command::new("bash")
            .arg(probe_health_script_path())
            .env("POISE_HEALTH_BASE_URL", format!("http://{bind_address}"))
            .env("POISE_HEALTH_INTERVAL_SECS", "1")
            .env("POISE_HEALTH_FAILURE_THRESHOLD", "2")
            .env("POISE_HEALTH_LOG_DIR", &log_dir)
            .env("POISE_LOG_DIR", &log_dir)
            .env("POISE_HEALTH_LOG", log_dir.join("health-probe.log"))
            .env("POISE_HEALTH_ALERT_HOOK", hook_command)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let status = wait_for_child_exit(&mut child).await;
        assert_eq!(status.code(), Some(3));

        let hook_output = fs::read_to_string(&hook_output_path).unwrap();
        assert_eq!(hook_output.trim(), "alert:2:503");

        let log_output = fs::read_to_string(log_dir.join("health-probe.log")).unwrap();
        assert!(log_output.contains("http_status=503"));
        assert!(log_output.contains("ALERT"));

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn startup_flow_serves_grid_list_and_detail() {
        let suffix = unique_test_environment();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind_address = listener.local_addr().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("test.toml");
        fs::write(
            &config_path,
            format!(
                r#"
environment = "{suffix}"
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
"#
            ),
        )
        .unwrap();

        let config = crate::config::load_config(config_path.to_str().unwrap()).unwrap();
        fs::create_dir_all(config.default_db_path().parent().unwrap()).unwrap();
        let storage = Arc::new(SqliteStorage::new(config.default_db_path()).unwrap());
        let platform = crate::assembly::assemble_with_components(
            &config,
            Arc::new(FakeExchange),
            Arc::new(FakeMarketData::default()),
            storage,
            Arc::new(FakeClock),
        )
        .await
        .unwrap();
        let runtime_handles = platform.runtime.start().await.unwrap();
        let app = crate::http::router(platform.state());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let tracks = client
            .get(format!("http://{bind_address}/tracks"))
            .send()
            .await
            .unwrap();
        assert!(tracks.status().is_success());
        let list: poise_protocol::TrackListResponse = tracks.json().await.unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].id, "btc-core");

        let detail = client
            .get(format!("http://{bind_address}/tracks/btc-core"))
            .send()
            .await
            .unwrap();
        assert!(detail.status().is_success());
        let payload: poise_protocol::TrackDetailView = detail.json().await.unwrap();
        assert_eq!(payload.identity.id, "btc-core");
        assert_eq!(payload.identity.instrument.symbol, "BTCUSDT");

        server.abort();
        let _ = server.await;
        runtime_handles.market_task.abort();
        runtime_handles.user_task.abort();
        runtime_handles.effect_task.abort();
        runtime_handles.recovery_task.abort();
        let _ = runtime_handles.market_task.await;
        let _ = runtime_handles.user_task.await;
        let _ = runtime_handles.effect_task.await;
        let _ = runtime_handles.recovery_task.await;
        let _ = fs::remove_dir_all(std::path::Path::new(".data").join(&suffix));
    }

    struct FakeExchange;

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Ok(OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            })
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Ok(())
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            Ok(Position {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            Ok(Vec::new())
        }

        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.1,
                    min_qty: 0.0,
                    min_notional: 0.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            })
        }

        async fn get_account_margin_snapshot(
            &self,
            instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountMarginSnapshot> {
            Ok(poise_engine::ports::AccountMarginSnapshot {
                venue: instrument.venue,
                available_balance: 1_000_000.0,
                total_wallet_balance: 1_000_000.0,
                max_increase_notional: 1_000_000.0,
                observed_at: Utc::now(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }

    #[derive(Default)]
    struct FakeMarketData {
        price_receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::MarketDataPort for FakeMarketData {
        async fn subscribe_prices(
            &self,
            instrument: &Instrument,
        ) -> Result<mpsc::Receiver<PriceTick>> {
            self.price_receivers
                .lock()
                .unwrap()
                .remove(&instrument.symbol)
                .ok_or_else(|| anyhow!("missing price receiver for {}", instrument.symbol))
        }

        async fn subscribe_user_data(
            &self,
        ) -> Result<mpsc::Receiver<poise_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }
}
