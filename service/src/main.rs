use std::{env, path::PathBuf, sync::Arc};

use anyhow::Context;
use clap::Parser;
use grid_platform_service::{
    Application, ApplicationRegistry, build_app,
    config::{InstanceConfig, ServiceConfig, default_sqlite_path},
    integrations::binance::{BinanceConfig, RealBinanceTransport, prepare_bootstrap_runtime},
    protocol::GridConfig,
    startup::{self, RuntimeMode, StartupDecision, StartupReport},
    storage::{PersistedRuntime, SqliteStorage},
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

    let startup = startup::StartupConfig::from_env()?;

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

    let router = if let Some(config) = loaded_config.as_ref() {
        let registry = registry_from_service_config(config)
            .await
            .with_context(|| "failed to bootstrap multi-instance registry from config")?;
        build_app(registry)
    } else {
        let binance_config = binance_config_from_env()?;
        let application = match (env::var("GRID_PLATFORM_SERVICE_DB_PATH"), binance_config) {
            (Ok(path), Some(config)) => {
                let transport = RealBinanceTransport::new(&config)
                    .with_context(|| "failed to build real binance transport")?;
                Application::bootstrap_with_startup_and_binance(path, config, Arc::new(transport))
                    .await
                    .with_context(|| "failed to bootstrap application with sqlite and binance")?
            }
            (Ok(path), None) => Application::bootstrap_with_sqlite(path)
                .with_context(|| "failed to bootstrap application with sqlite storage")?,
            (Err(_), Some(config)) => {
                let transport = RealBinanceTransport::new(&config)
                    .with_context(|| "failed to build real binance transport")?;
                Application::bootstrap_with_startup_and_binance(
                    startup.db_path.clone(),
                    config,
                    Arc::new(transport),
                )
                .await
                .with_context(
                    || "failed to bootstrap application with derived sqlite and binance",
                )?
            }
            (Err(_), None) => Application::bootstrap_with_sqlite(startup.db_path.clone())
                .with_context(|| "failed to bootstrap application with derived sqlite storage")?,
        };
        build_app(application)
    };
    info!("grid-platform service listening on {addr}");
    axum::serve(listener, router)
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

async fn registry_from_service_config(
    config: &ServiceConfig,
) -> anyhow::Result<ApplicationRegistry> {
    let binance = binance_registry_options_from_env();
    let mut instances = Vec::with_capacity(config.instances.len());
    for instance in &config.instances {
        instances.push(bootstrap_instance_application(config, instance, binance.as_ref()).await?);
    }
    let default_symbol = config
        .default_symbol
        .clone()
        .unwrap_or_else(|| config.instances[0].symbol.clone());
    ApplicationRegistry::new(config.environment.clone(), default_symbol, instances)
}

async fn bootstrap_instance_application(
    config: &ServiceConfig,
    instance: &InstanceConfig,
    binance: Option<&BinanceRegistryOptions>,
) -> anyhow::Result<(String, Application)> {
    let db_path = default_sqlite_path(&config.environment, &instance.symbol);
    let storage = SqliteStorage::open(&db_path)
        .with_context(|| format!("failed to open sqlite for {}", instance.symbol))?;
    let runtime = storage
        .load_runtime()?
        .unwrap_or_else(PersistedRuntime::sqlite_bootstrap);
    let runtime = apply_instance_config(runtime, config, instance);

    let application = if let Some(binance) = binance {
        let binance_config = binance.config_for_symbol(&config.environment, &instance.symbol);
        let startup_mode = if config.environment.eq_ignore_ascii_case("mainnet") {
            RuntimeMode::Mainnet
        } else {
            RuntimeMode::Testnet
        };
        let transport =
            Arc::new(RealBinanceTransport::new(&binance_config).with_context(|| {
                format!(
                    "failed to build real binance transport for {}",
                    instance.symbol
                )
            })?);
        let startup =
            StartupReport::collect(startup_mode, &instance.symbol, &runtime, transport.clone())
                .await?;

        let mut runtime = prepare_bootstrap_runtime(runtime, &binance_config);
        runtime.snapshot.execution.open_orders_source =
            grid_platform_service::protocol::OpenOrdersSource::StrategyMirror;
        let runtime = startup.apply_to(runtime);
        storage.persist_runtime(&runtime)?;
        if let StartupDecision::Refuse { code, message } = &startup.decision {
            anyhow::bail!("{code}: {message}");
        }

        Application::bootstrap_with_runtime_storage_and_binance(
            runtime,
            Some(storage),
            binance_config,
            transport,
            instance.symbol.clone(),
        )
    } else {
        storage.persist_runtime(&runtime)?;
        Application::bootstrap_with_runtime_and_storage(
            runtime,
            Some(storage),
            instance.symbol.clone(),
        )
    };

    Ok((instance.symbol.clone(), application))
}

fn apply_instance_config(
    mut runtime: PersistedRuntime,
    config: &ServiceConfig,
    instance: &InstanceConfig,
) -> PersistedRuntime {
    runtime.snapshot.runtime.symbol = instance.symbol.clone();
    runtime.snapshot.runtime.env = config.environment.clone();
    runtime.snapshot.strategy.config = GridConfig {
        lower_price: instance.range.lower_price,
        upper_price: instance.range.upper_price,
        grid_levels: instance.range.grid_levels,
        max_position_notional: instance.range.max_position_notional,
    };
    runtime
}

#[derive(Debug, Clone)]
struct BinanceRegistryOptions {
    rest_base_url: Option<String>,
    ws_base_url: Option<String>,
    api_key: Option<String>,
    api_secret: Option<String>,
}

impl BinanceRegistryOptions {
    fn config_for_symbol(&self, environment: &str, symbol: &str) -> BinanceConfig {
        let mut config = if environment.eq_ignore_ascii_case("mainnet") {
            BinanceConfig::mainnet(symbol.to_string())
        } else {
            BinanceConfig::testnet(symbol.to_string())
        };
        if let Some(rest_base_url) = &self.rest_base_url {
            config.rest_base_url = rest_base_url.clone();
        }
        if let Some(ws_base_url) = &self.ws_base_url {
            config.ws_base_url = ws_base_url.clone();
        }
        config.api_key = self.api_key.clone();
        config.api_secret = self.api_secret.clone();
        config
    }
}

fn binance_registry_options_from_env() -> Option<BinanceRegistryOptions> {
    let enabled = env::var("GRID_PLATFORM_BINANCE_ENABLED")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    if !enabled {
        return None;
    }

    Some(BinanceRegistryOptions {
        rest_base_url: env::var("GRID_PLATFORM_BINANCE_REST_BASE_URL").ok(),
        ws_base_url: env::var("GRID_PLATFORM_BINANCE_WS_BASE_URL").ok(),
        api_key: env::var("GRID_PLATFORM_BINANCE_API_KEY").ok(),
        api_secret: env::var("GRID_PLATFORM_BINANCE_API_SECRET").ok(),
    })
}
