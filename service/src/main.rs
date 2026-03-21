use std::{env, path::PathBuf};

use anyhow::Context;
use clap::Parser;
use grid_platform_service::{
    Application, build_app,
    config::ServiceConfig,
    integrations::binance::{BinanceConfig, RealBinanceTransport},
};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "grid-platform-service",
    version,
    about = "网格平台服务端",
    long_about = None
)]
struct Cli {
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("grid_platform_service=info,tower_http=info")),
        )
        .init();

    let loaded_config = cli
        .config
        .as_ref()
        .map(|path| {
            ServiceConfig::load_from_path(path)
                .with_context(|| format!("failed to load config from --config {}", path.display()))
        })
        .transpose()?;
    if let Some(config) = &loaded_config {
        info!(
            environment = %config.environment,
            instances = config.instances.len(),
            default_symbol = ?config.default_symbol,
            "loaded service config from --config"
        );
    }

    let addr = env::var("GRID_PLATFORM_SERVICE_ADDR").unwrap_or_else(|_| "127.0.0.1:8000".into());
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    let binance_config = binance_config_from_env()?;
    let application = match (env::var("GRID_PLATFORM_SERVICE_DB_PATH"), binance_config) {
        (Ok(path), Some(config)) => {
            let transport = RealBinanceTransport::new(&config)
                .with_context(|| "failed to build real binance transport")?;
            Application::bootstrap_with_sqlite_and_binance(
                path,
                config,
                std::sync::Arc::new(transport),
            )
            .with_context(|| "failed to bootstrap application with sqlite and binance")?
        }
        (Ok(path), None) => Application::bootstrap_with_sqlite(path)
            .with_context(|| "failed to bootstrap application with sqlite storage")?,
        (Err(_), Some(config)) => {
            let transport = RealBinanceTransport::new(&config)
                .with_context(|| "failed to build real binance transport")?;
            Application::bootstrap_with_binance(config, std::sync::Arc::new(transport))
        }
        (Err(_), None) => Application::bootstrap(),
    };
    info!("grid-platform service listening on {addr}");
    axum::serve(listener, build_app(application))
        .await
        .context("service stopped unexpectedly")
}

fn binance_config_from_env() -> anyhow::Result<Option<BinanceConfig>> {
    let enabled = env::var("GRID_PLATFORM_BINANCE_ENABLED")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    if !enabled {
        return Ok(None);
    }

    let symbol = env::var("GRID_PLATFORM_BINANCE_SYMBOL").unwrap_or_else(|_| "XAUUSDT".into());
    let env_name = env::var("GRID_PLATFORM_BINANCE_ENV").unwrap_or_else(|_| "testnet".into());
    let mut config = if env_name.eq_ignore_ascii_case("mainnet") {
        BinanceConfig::mainnet(symbol)
    } else {
        BinanceConfig::testnet(symbol)
    };
    if let Ok(rest_base_url) = env::var("GRID_PLATFORM_BINANCE_REST_BASE_URL") {
        config.rest_base_url = rest_base_url;
    }
    if let Ok(ws_base_url) = env::var("GRID_PLATFORM_BINANCE_WS_BASE_URL") {
        config.ws_base_url = ws_base_url;
    }
    config.api_key = env::var("GRID_PLATFORM_BINANCE_API_KEY").ok();
    config.api_secret = env::var("GRID_PLATFORM_BINANCE_API_SECRET").ok();
    Ok(Some(config))
}
