mod account_projector;
mod assembly;
mod config;
mod effect_worker;
mod event_presentation;
mod exchange;
mod exchange_freshness;
mod http;
mod instance_dir;
mod order_outcome;
#[allow(dead_code)]
mod projector;
mod runtime;
mod server_context;
mod state_bootstrap;
mod submit_preflight;
#[cfg(test)]
mod test_support;
mod websocket;

use std::env;

use anyhow::Result;
use state_bootstrap::{
    PersistedStateMismatchDetail, StateBootstrapError, StateBootstrapMode, SuggestedAction,
};

use crate::instance_dir::InstanceDir;

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupOptions {
    instance_dir: std::path::PathBuf,
    bootstrap_mode: StateBootstrapMode,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Poise server starting");

    let options = parse_startup_options(env::args().skip(1))?;
    let instance_dir = InstanceDir::new(&options.instance_dir);
    let config = config::load_config(instance_dir.config_path())?;
    let db_path = instance_dir.db_path();
    let prepared_state =
        match state_bootstrap::prepare_state_repository(&config, &db_path, options.bootstrap_mode)
            .await
        {
            Ok(repository) => repository,
            Err(StateBootstrapError::Unexpected(error)) => return Err(error),
            Err(error) => return Err(anyhow::anyhow!(render_startup_error(&error))),
        };
    let (platform, runtime_handles, listener) = prepared_state
        .run_startup(|repositories| async {
            let platform = assembly::assemble(&config, repositories).await?;
            start_platform(&config, platform).await
        })
        .await?;
    let app = http::router(platform.http_state(), platform.websocket_state());
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    platform.runtime.shutdown(runtime_handles).await;
    serve_result?;

    Ok(())
}

async fn start_platform(
    config: &config::Config,
    platform: assembly::ServerPlatform,
) -> Result<(
    assembly::ServerPlatform,
    runtime::RuntimeHandles,
    tokio::net::TcpListener,
)> {
    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    let runtime_handles = platform.runtime.start().await?;
    Ok((platform, runtime_handles, listener))
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
    let mut instance_dir = None;
    let mut bootstrap_mode = StateBootstrapMode::Strict;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--instance-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --instance-dir"))?;
                instance_dir = Some(std::path::PathBuf::from(value));
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
        instance_dir: instance_dir
            .ok_or_else(|| anyhow::anyhow!("missing required --instance-dir <path>"))?,
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
                match &mismatch.detail {
                    PersistedStateMismatchDetail::DefinitionChanged {
                        expected_instrument,
                        actual_instrument,
                        expected_config,
                        actual_config,
                    } => {
                        let instrument_line = if expected_instrument != actual_instrument {
                            format!(
                                "\n  instrument: expected `{}:{}`, persisted `{}:{}`",
                                expected_instrument.venue.as_str(),
                                expected_instrument.symbol,
                                actual_instrument.venue.as_str(),
                                actual_instrument.symbol
                            )
                        } else {
                            String::new()
                        };
                        rendered.push_str(&format!(
                            "\ntrack `{}`:{}\n  expected config: {}\n  persisted config: {}",
                            mismatch.track_id,
                            instrument_line,
                            serde_json::to_string(expected_config)
                                .expect("track config should serialize"),
                            serde_json::to_string(actual_config)
                                .expect("track config should serialize"),
                        ));
                    }
                    PersistedStateMismatchDetail::PersistedTrackMissingFromConfig {
                        actual_instrument,
                        actual_config,
                    } => {
                        rendered.push_str(&format!(
                            "\ntrack `{}`:\n  persisted instrument: `{}:{}`\n  persisted config: {}\n  config status: missing from current config",
                            mismatch.track_id,
                            actual_instrument.venue.as_str(),
                            actual_instrument.symbol,
                            serde_json::to_string(actual_config)
                                .expect("track config should serialize"),
                        ));
                    }
                }
            }

            match suggested_action {
                SuggestedAction::RebuildState => {
                    rendered.push_str(
                        "\nuse `--rebuild-state` to back up the old database, discard local snapshots, and rebuild state from the exchange's live positions and orders.\n\
suggested command: cargo run -p poise-server -- --instance-dir <path> --rebuild-state",
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
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use chrono::Utc;
    use poise_core::types::ExchangeRules;
    use poise_engine::ports::{
        AccountPort, ClockPort, ExchangeInfo, ExchangeOrder, ExecutionPort, MetadataPort,
        OrderReceipt, OrderRequest, OrderStatus, Position, PriceTick,
    };
    use poise_engine::track::{Instrument, Venue};
    use poise_storage::sqlite::SqliteStorage;
    use tokio::sync::mpsc;

    use crate::state_bootstrap::{
        PersistedStateMismatchDetail, StateBootstrapError, StateBootstrapMode, StateRepositories,
        SuggestedAction,
    };

    use super::{StartupOptions, parse_startup_options};

    #[test]
    fn parse_startup_options_requires_instance_dir() {
        let error = parse_startup_options(Vec::<String>::new().into_iter()).unwrap_err();
        assert!(error.to_string().contains("--instance-dir"));
    }

    #[test]
    fn parse_startup_options_reads_instance_dir_value() {
        let options = parse_startup_options(
            vec!["--instance-dir".to_string(), "/tmp/poise-a".to_string()].into_iter(),
        )
        .unwrap();
        assert_eq!(
            options,
            StartupOptions {
                instance_dir: std::path::PathBuf::from("/tmp/poise-a"),
                bootstrap_mode: StateBootstrapMode::Strict,
            }
        );
    }

    #[test]
    fn parse_startup_options_accepts_rebuild_state_flag() {
        let options = parse_startup_options(
            vec![
                "--rebuild-state".to_string(),
                "--instance-dir".to_string(),
                "/tmp/poise-a".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();

        assert_eq!(
            options,
            StartupOptions {
                instance_dir: std::path::PathBuf::from("/tmp/poise-a"),
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
                detail: PersistedStateMismatchDetail::DefinitionChanged {
                    expected_instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    actual_instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    expected_config: poise_core::strategy::TrackConfig {
                        lower_price: 90.0,
                        upper_price: 110.0,
                        long_exposure_units: 8.0,
                        short_exposure_units: 8.0,
                        notional_per_unit: 375.0,
                        min_rebalance_units: 0.5,
                        shape_family: poise_core::strategy::ShapeFamily::Linear,
                        out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                    },
                    actual_config: poise_core::strategy::TrackConfig {
                        lower_price: 80.0,
                        upper_price: 110.0,
                        long_exposure_units: 8.0,
                        short_exposure_units: 8.0,
                        notional_per_unit: 375.0,
                        min_rebalance_units: 0.5,
                        shape_family: poise_core::strategy::ShapeFamily::Linear,
                        out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                    },
                },
            }],
            suggested_action: SuggestedAction::RebuildState,
        });

        assert!(rendered.contains(".data/testnet/poise-server.sqlite"));
        assert!(rendered.contains("btc-core"));
        assert!(rendered.contains("--rebuild-state"));
        assert!(rendered.contains("--instance-dir <path>"));
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

    fn run_instance_server_script_path() -> PathBuf {
        workspace_root()
            .join("scripts")
            .join("run-instance-server.sh")
    }

    fn start_instance_zellij_script_path() -> PathBuf {
        workspace_root()
            .join("scripts")
            .join("start-instance-zellij.sh")
    }

    fn instance_zellij_layout_path() -> PathBuf {
        workspace_root()
            .join("ops")
            .join("zellij")
            .join("poise-instance.kdl")
    }

    #[test]
    fn run_instance_server_script_dry_run_uses_instance_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let output = Command::new("bash")
            .arg(run_instance_server_script_path())
            .arg("--dry-run")
            .env("POISE_INSTANCE_DIR", temp_dir.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("instance_dir="));
        assert!(stdout.contains("--instance-dir"));
        assert!(stdout.contains(".logs/poise-server.log"));
    }

    #[test]
    fn run_instance_server_script_dry_run_normalizes_relative_instance_dir() {
        let caller_dir = tempfile::tempdir().unwrap();
        let instance_dir = caller_dir.path().join("instances").join("alpha");
        fs::create_dir_all(&instance_dir).unwrap();

        let output = Command::new("bash")
            .arg(run_instance_server_script_path())
            .arg("--dry-run")
            .env("POISE_INSTANCE_DIR", "instances/alpha")
            .current_dir(caller_dir.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let rendered_instance_dir = stdout
            .lines()
            .find_map(|line| line.strip_prefix("instance_dir="))
            .unwrap();
        assert!(std::path::Path::new(rendered_instance_dir).is_absolute());
        assert_eq!(
            fs::canonicalize(rendered_instance_dir).unwrap(),
            fs::canonicalize(&instance_dir).unwrap()
        );
    }

    #[test]
    fn start_instance_zellij_dry_run_exports_instance_dir_and_base_url() {
        let temp_dir = tempfile::tempdir().unwrap();
        let output = Command::new("bash")
            .arg(start_instance_zellij_script_path())
            .arg("--dry-run")
            .env("POISE_INSTANCE_DIR", temp_dir.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("instance_dir="));
        assert!(stdout.contains("base_url="));
        assert!(!stdout.contains("paper"));
    }

    #[test]
    fn start_instance_zellij_dry_run_normalizes_relative_instance_dir() {
        let caller_dir = tempfile::tempdir().unwrap();
        let instance_dir = caller_dir.path().join("instances").join("beta");
        fs::create_dir_all(&instance_dir).unwrap();

        let output = Command::new("bash")
            .arg(start_instance_zellij_script_path())
            .arg("--dry-run")
            .env("POISE_INSTANCE_DIR", "instances/beta")
            .current_dir(caller_dir.path())
            .output()
            .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let rendered_instance_dir = stdout
            .lines()
            .find_map(|line| line.strip_prefix("instance_dir="))
            .unwrap();
        assert!(std::path::Path::new(rendered_instance_dir).is_absolute());
        assert_eq!(
            fs::canonicalize(rendered_instance_dir).unwrap(),
            fs::canonicalize(&instance_dir).unwrap()
        );
    }

    #[test]
    fn instance_zellij_layout_quotes_repo_root_script_paths() {
        let layout = fs::read_to_string(instance_zellij_layout_path()).unwrap();

        assert!(layout.contains("exec \\\"$POISE_REPO_ROOT/scripts/run-instance-tui.sh\\\""));
        assert!(layout.contains("exec \\\"$POISE_REPO_ROOT/scripts/run-instance-server.sh\\\""));
        assert!(layout.contains("exec \\\"$POISE_REPO_ROOT/scripts/probe-health.sh\\\""));
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
            .env("POISE_BASE_URL", format!("http://{bind_address}"))
            .env("POISE_INSTANCE_DIR", temp_dir.path())
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
    async fn probe_health_once_succeeds_without_instance_dir_or_log_dir() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind_address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/health",
            get(|| async {
                (
                    StatusCode::OK,
                    r#"{"status":"ok","track_count":1,"attention_required_count":0}"#,
                )
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        for _ in 0..20 {
            if tokio::net::TcpStream::connect(bind_address).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let script_path = probe_health_script_path();
        let base_url = format!("http://{bind_address}");
        let output = tokio::task::spawn_blocking(move || {
            Command::new("bash")
                .arg(script_path)
                .arg("--once")
                .env("POISE_BASE_URL", base_url)
                .output()
                .unwrap()
        })
        .await
        .unwrap();

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("http_status=200"));

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn startup_flow_serves_track_list_and_detail() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind_address = listener.local_addr().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let instance_dir = temp_dir.path().join("instance-a");
        fs::create_dir_all(&instance_dir).unwrap();
        let config_path = instance_dir.join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
bind_address = "{bind_address}"

[exchange]
venue = "binance"
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
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

        let config = crate::config::load_config(&config_path).unwrap();
        let db_path = crate::instance_dir::InstanceDir::new(&instance_dir).db_path();
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let storage = Arc::new(SqliteStorage::new(&db_path).unwrap());
        let exchange = Arc::new(FakeExchange);
        let platform = crate::assembly::assemble_with_exchange_ports(
            &config,
            exchange.clone(),
            Arc::new(FakeMarketData::default()),
            exchange.clone(),
            exchange.clone(),
            exchange,
            StateRepositories::new(storage),
            Arc::new(FakeClock),
        )
        .await
        .unwrap();
        let runtime_handles = platform.runtime.start().await.unwrap();
        let app = crate::http::router(platform.http_state(), platform.websocket_state());
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
    }

    #[test]
    fn startup_db_path_does_not_depend_on_exchange_deployment() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config_path = instance_dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
bind_address = "127.0.0.1:8000"

[exchange]
venue = "binance"
deployment = "testnet"
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap();
        let testnet_config = crate::config::load_config(&config_path).unwrap();
        let testnet_path = crate::instance_dir::InstanceDir::new(instance_dir.path()).db_path();

        fs::write(
            &config_path,
            r#"
bind_address = "127.0.0.1:8000"

[exchange]
venue = "binance"
deployment = "mainnet"
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap();
        let mainnet_config = crate::config::load_config(&config_path).unwrap();
        let mainnet_path = crate::instance_dir::InstanceDir::new(instance_dir.path()).db_path();

        assert_eq!(testnet_config.bind_address, mainnet_config.bind_address);
        assert_eq!(testnet_path, mainnet_path);
        assert_eq!(
            mainnet_path,
            instance_dir
                .path()
                .join(".data")
                .join("poise-server.sqlite")
        );
    }

    #[tokio::test]
    async fn start_platform_binds_listener_before_starting_runtime() {
        let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind_address = occupied_listener.local_addr().unwrap();

        let config = crate::config::Config {
            bind_address: bind_address.to_string(),
            tracks: vec![crate::config::TrackDefinition {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: poise_core::strategy::ShapeFamily::Linear,
                out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                max_notional: None,
                daily_loss_limit: None,
                stop_loss_pct: None,
                tick_timeout_secs: None,
            }],
            exchange: crate::config::ExchangeConfig::Binance(poise_binance::Config {
                api_key: Some("demo-key".into()),
                api_secret: Some("demo-secret".into()),
                ..Default::default()
            }),
            account_monitor: Default::default(),
        };
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = crate::instance_dir::InstanceDir::new(temp_dir.path()).db_path();
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let storage = Arc::new(SqliteStorage::new(&db_path).unwrap());
        let exchange = Arc::new(FakeExchange);
        let platform = crate::assembly::assemble_with_exchange_ports(
            &config,
            exchange.clone(),
            Arc::new(FailingStartMarketData),
            exchange.clone(),
            exchange.clone(),
            exchange,
            StateRepositories::new(storage),
            Arc::new(FakeClock),
        )
        .await
        .unwrap();

        let error = super::start_platform(&config, platform)
            .await
            .err()
            .unwrap();

        assert!(
            !error
                .to_string()
                .contains("subscribe_user_data should not run")
        );
    }

    struct FakeExchange;

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountSummaryPort for FakeExchange {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: Utc::now(),
            })
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for FakeExchange {
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
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeExchange {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
            Ok(poise_engine::ports::AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn subscribe_user_data(
            &self,
        ) -> Result<mpsc::Receiver<poise_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    #[async_trait::async_trait]
    impl MetadataPort for FakeExchange {
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
    }

    struct FailingStartMarketData;

    #[async_trait::async_trait]
    impl poise_engine::ports::MarketDataPort for FailingStartMarketData {
        async fn subscribe_prices(
            &self,
            _instrument: &Instrument,
        ) -> Result<mpsc::Receiver<PriceTick>> {
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
