use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use poise_binance::BinanceAdapter;
use poise_engine::manager::TrackManager;
use poise_engine::ports::{
    ClockPort, ExchangePort, MarketDataPort, StateRepositoryPort, TrackReadRepositoryPort,
};
use poise_engine::track::{Instrument, TrackId};
use poise_storage::sqlite::SqliteStorage;
use tokio::sync::broadcast;
use tokio::time::{Duration, sleep};

use crate::config::Config;
use crate::projector::TrackProjector;
use crate::query_service::TrackQueryService;
use crate::runtime::{
    AccountMarginGuardStore, RuntimeHandles, ServerRuntime, TrackReconcileGuards,
};
use crate::write_service::TrackWriteService;
#[derive(Clone)]
pub struct ServerState {
    pub write_service: Arc<TrackWriteService>,
    pub state_repository: Arc<dyn StateRepositoryPort>,
    #[allow(dead_code)]
    pub query_service: Arc<TrackQueryService>,
    #[allow(dead_code)]
    pub projector: Arc<TrackProjector>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
    pub reconcile_guards: Arc<TrackReconcileGuards>,
}

pub struct ServerPlatform {
    state: ServerState,
    pub runtime: ServerRuntime,
}

#[derive(Debug)]
struct ValidatedExchangeRuntimeConfig {
    api_key: String,
    api_secret: String,
    rest_base_url: String,
    ws_base_url: String,
}

const STARTUP_RETRY_ATTEMPTS: usize = 5;
#[cfg(test)]
const STARTUP_RETRY_DELAY: Duration = Duration::from_millis(1);
#[cfg(not(test))]
const STARTUP_RETRY_DELAY: Duration = Duration::from_secs(1);

fn validate_unique_instruments(
    instruments: impl IntoIterator<Item = Instrument>,
) -> Result<(), anyhow::Error> {
    let mut known_instruments = HashSet::new();
    for instrument in instruments {
        if !known_instruments.insert(instrument.clone()) {
            return Err(anyhow!(
                "duplicate instrument `{}:{}`",
                instrument.venue.as_str(),
                instrument.symbol
            ));
        }
    }
    Ok(())
}

fn validate_unique_track_ids(
    track_ids: impl IntoIterator<Item = TrackId>,
) -> Result<(), anyhow::Error> {
    let mut known_track_ids = HashSet::new();
    for track_id in track_ids {
        if !known_track_ids.insert(track_id.as_str().to_string()) {
            return Err(anyhow!("duplicate grid id `{}`", track_id.as_str()));
        }
    }
    Ok(())
}

fn required_exchange_field(value: Option<&str>, field_name: &str) -> Result<String> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing required {field_name}"))?;
    Ok(value.to_string())
}

fn validate_exchange_runtime_config(config: &Config) -> Result<ValidatedExchangeRuntimeConfig> {
    let api_key = required_exchange_field(config.exchange.api_key.as_deref(), "exchange.api_key")?;
    let api_secret =
        required_exchange_field(config.exchange.api_secret.as_deref(), "exchange.api_secret")?;
    let (rest_base_url, ws_base_url) = resolve_binance_endpoints(&config.environment)?;
    Ok(ValidatedExchangeRuntimeConfig {
        rest_base_url,
        ws_base_url,
        api_key,
        api_secret,
    })
}

fn resolve_binance_endpoints(environment: &str) -> Result<(String, String)> {
    resolve_binance_endpoints_with_lookup(environment, |name| env::var(name).ok())
}

fn resolve_binance_endpoints_with_lookup(
    environment: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<(String, String)> {
    if environment.eq_ignore_ascii_case("testnet") {
        return Ok((
            "https://demo-fapi.binance.com".to_string(),
            "wss://fstream.binancefuture.com".to_string(),
        ));
    }
    if environment.eq_ignore_ascii_case("mainnet") {
        return Ok((
            "https://fapi.binance.com".to_string(),
            "wss://fstream.binance.com".to_string(),
        ));
    }
    if environment.eq_ignore_ascii_case("test") {
        let rest_base_url = lookup("POISE_TEST_BINANCE_REST_BASE_URL");
        let ws_base_url = lookup("POISE_TEST_BINANCE_WS_BASE_URL");
        if let (Some(rest_base_url), Some(ws_base_url)) = (rest_base_url, ws_base_url) {
            return Ok((rest_base_url, ws_base_url));
        }
        return Err(anyhow!(
            "environment `test` is reserved for automated tests; set `POISE_TEST_BINANCE_REST_BASE_URL` and `POISE_TEST_BINANCE_WS_BASE_URL` to start the real Binance runtime in test mode"
        ));
    }

    Err(anyhow!(
        "unsupported runtime environment `{environment}`; expected `testnet` or `mainnet`"
    ))
}

pub async fn assemble(config: &Config) -> Result<ServerPlatform> {
    validate_unique_instruments(config.tracks.iter().map(|track| track.instrument()))?;
    validate_unique_track_ids(config.tracks.iter().map(|track| track.track_id()))?;
    let exchange_config = validate_exchange_runtime_config(config)?;

    let adapter = Arc::new(BinanceAdapter::new(
        exchange_config.api_key,
        exchange_config.api_secret,
        exchange_config.rest_base_url,
        exchange_config.ws_base_url,
    ));
    let exchange: Arc<dyn ExchangePort> = adapter.clone();
    let market_data: Arc<dyn MarketDataPort> = adapter;

    let db_path = config.default_db_path();
    ensure_parent_dir(&db_path)?;
    let storage = Arc::new(SqliteStorage::new(&db_path)?);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock);

    assemble_with_components_with_repository(config, exchange, market_data, storage, clock).await
}

#[cfg(test)]
pub(crate) async fn assemble_with_components<R>(
    config: &Config,
    exchange: Arc<dyn ExchangePort>,
    market_data: Arc<dyn MarketDataPort>,
    repository: Arc<R>,
    clock: Arc<dyn ClockPort>,
) -> Result<ServerPlatform>
where
    R: StateRepositoryPort + TrackReadRepositoryPort + 'static,
{
    assemble_with_components_with_repository(config, exchange, market_data, repository, clock).await
}

async fn assemble_with_components_with_repository<R>(
    config: &Config,
    exchange: Arc<dyn ExchangePort>,
    market_data: Arc<dyn MarketDataPort>,
    repository: Arc<R>,
    clock: Arc<dyn ClockPort>,
) -> Result<ServerPlatform>
where
    R: StateRepositoryPort + TrackReadRepositoryPort + 'static,
{
    validate_unique_instruments(config.tracks.iter().map(|track| track.instrument()))?;
    validate_unique_track_ids(config.tracks.iter().map(|track| track.track_id()))?;

    let mut manager = TrackManager::new(clock);
    let mut account_margin_snapshots = HashMap::new();
    for track in &config.tracks {
        let track_id = track.track_id();
        let info = load_exchange_info_with_retry(exchange.as_ref(), &track.instrument()).await?;
        let account_margin_snapshot =
            load_account_margin_snapshot_with_retry(exchange.as_ref(), &track.instrument()).await?;
        if track.budget().max_notional > account_margin_snapshot.max_increase_notional {
            return Err(anyhow!(
                "insufficient account margin for configured max_notional on track `{}`: required {}, available {}",
                track_id.as_str(),
                track.budget().max_notional,
                account_margin_snapshot.max_increase_notional
            ));
        }
        account_margin_snapshots.insert(track.instrument(), account_margin_snapshot);
        manager.add_track_with_tick_timeout_secs(
            track_id.clone(),
            track.instrument(),
            track.track_config(),
            track.budget(),
            info.rules,
            track.tick_timeout_secs(),
        )?;
        if let Some(snapshot) = repository.load_track_state(track_id.as_str()).await? {
            manager.restore_track_state(&snapshot)?;
        }
    }

    let (notifications, _) = broadcast::channel(256);
    let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
    let read_repository: Arc<dyn TrackReadRepositoryPort> = repository;
    let write_service = Arc::new(TrackWriteService::new(
        manager,
        state_repository.clone(),
        notifications.clone(),
    ));
    let query_service = Arc::new(TrackQueryService::new(read_repository));
    let projector = Arc::new(TrackProjector::new());
    let server_state =
        build_server_state(write_service, state_repository, query_service, projector);

    Ok(ServerPlatform {
        state: server_state.clone(),
        runtime: ServerRuntime::with_account_margin_snapshots(
            server_state,
            exchange,
            market_data,
            account_margin_snapshots,
            Duration::from_secs(1),
        ),
    })
}

async fn load_exchange_info_with_retry(
    exchange: &dyn ExchangePort,
    instrument: &Instrument,
) -> Result<poise_engine::ports::ExchangeInfo> {
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match exchange.get_exchange_info(instrument).await {
            Ok(info) => return Ok(info),
            Err(error) => {
                if attempt + 1 == STARTUP_RETRY_ATTEMPTS {
                    return Err(error);
                }
                tracing::warn!(
                    instrument = %instrument.symbol,
                    attempt = attempt + 1,
                    max_attempts = STARTUP_RETRY_ATTEMPTS,
                    "startup exchange info probe failed: {error}"
                );
                last_error = Some(error);
            }
        }

        sleep(STARTUP_RETRY_DELAY).await;
    }

    Err(last_error.unwrap_or_else(|| anyhow!("failed to load exchange info")))
}

async fn load_account_margin_snapshot_with_retry(
    exchange: &dyn ExchangePort,
    instrument: &Instrument,
) -> Result<poise_engine::ports::AccountMarginSnapshot> {
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match exchange.get_account_margin_snapshot(instrument).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => {
                if attempt + 1 == STARTUP_RETRY_ATTEMPTS {
                    return Err(error);
                }
                tracing::warn!(
                    instrument = %instrument.symbol,
                    attempt = attempt + 1,
                    max_attempts = STARTUP_RETRY_ATTEMPTS,
                    "startup account margin probe failed: {error}"
                );
                last_error = Some(error);
            }
        }

        sleep(STARTUP_RETRY_DELAY).await;
    }

    Err(last_error.unwrap_or_else(|| anyhow!("failed to load account margin snapshot")))
}

impl ServerPlatform {
    pub fn state(&self) -> ServerState {
        self.state.clone()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn start_market_data_tasks(&self) -> Result<RuntimeHandles> {
        self.runtime.start().await
    }
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("database path `{}` has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create database directory `{}`", parent.display()))
}

pub(crate) struct SystemClock;

impl ClockPort for SystemClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        Utc::now()
    }
}

pub(crate) fn build_server_state(
    write_service: Arc<TrackWriteService>,
    state_repository: Arc<dyn StateRepositoryPort>,
    query_service: Arc<TrackQueryService>,
    projector: Arc<TrackProjector>,
) -> ServerState {
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    write_service.set_account_margin_guard(account_margin_guard.clone());
    ServerState {
        write_service,
        state_repository,
        query_service,
        projector,
        account_margin_guard,
        reconcile_guards: Arc::new(TrackReconcileGuards::default()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use futures_util::StreamExt;
    use poise_core::events::DomainEvent as EngineDomainEvent;
    use poise_engine::manager::TrackManager;
    use poise_engine::observation::{MarketObservation, TrackObservation};
    use poise_engine::ports::{
        ExchangeInfo, ExchangeOrder, ExchangePort, OrderReceipt, OrderRequest, Position, PriceTick,
        StateRepositoryPort, TrackReadRepositoryPort, TrackSnapshot,
    };
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_protocol::{TrackStreamEvent, TrackStreamPayload};
    use poise_storage::sqlite::SqliteStorage;
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};
    use tokio_tungstenite::connect_async;

    use crate::config::{Config, ExchangeConfig, TrackDefinition};
    use crate::http::router;
    use crate::projector::TrackProjector;
    use crate::query_service::TrackQueryService;
    use crate::write_service::TrackWriteService;

    use super::{
        ServerPlatform, SystemClock, assemble, build_server_state, resolve_binance_endpoints,
        resolve_binance_endpoints_with_lookup, validate_exchange_runtime_config,
        validate_unique_instruments, validate_unique_track_ids,
    };

    fn test_exchange_rules() -> poise_core::types::ExchangeRules {
        poise_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    fn unique_test_environment() -> String {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        format!(
            "assembly-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[tokio::test]
    async fn assembles_platform_with_all_instances_registered() {
        let suffix = unique_test_environment();

        let config = Config {
            environment: suffix.clone(),
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![
                TrackDefinition {
                    track_id: "btc-core".into(),
                    venue: Venue::Binance,
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
                },
                TrackDefinition {
                    track_id: "eth-core".into(),
                    venue: Venue::Binance,
                    symbol: "ETHUSDT".into(),
                    lower_price: 2000.0,
                    upper_price: 2500.0,
                    long_exposure_units: 5.0,
                    short_exposure_units: 3.0,
                    notional_per_unit: 500.0,
                    min_rebalance_units: 0.5,
                    shape_family: poise_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                    max_notional: None,
                    daily_loss_limit: None,
                    stop_loss_pct: None,
                    tick_timeout_secs: None,
                },
            ],
            exchange: ExchangeConfig {
                ..Default::default()
            },
        };

        let platform = assemble_with_fake_ports(&config).await.unwrap();
        let manager_handle = platform.state().write_service.manager();
        let manager = manager_handle.read().await;

        assert_eq!(manager.list_tracks().len(), 2);
        let grid = manager.get_track("btc-core").unwrap();
        assert_eq!(grid.budget().max_notional, 3000.0);
        assert!(
            std::path::Path::new(".data")
                .join(&suffix)
                .join("poise-server.sqlite")
                .exists()
        );

        let _ = std::fs::remove_dir_all(std::path::Path::new(".data").join(&suffix));
    }

    #[test]
    fn assemble_rejects_duplicate_track_ids() {
        let error =
            validate_unique_track_ids([TrackId::new("alpha"), TrackId::new("alpha")]).unwrap_err();
        assert!(error.to_string().contains("duplicate grid id"));
    }

    #[test]
    fn assemble_rejects_duplicate_instruments() {
        let error = validate_unique_instruments([
            Instrument::new(Venue::Binance, "BTCUSDT"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("duplicate instrument"));
    }

    #[tokio::test]
    async fn assemble_rejects_duplicate_symbols() {
        let suffix = unique_test_environment();

        let config = Config {
            environment: suffix,
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![
                TrackDefinition {
                    track_id: "btc-core".into(),
                    venue: Venue::Binance,
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
                },
                TrackDefinition {
                    track_id: "btc-alt".into(),
                    venue: Venue::Binance,
                    symbol: "BTCUSDT".into(),
                    lower_price: 80.0,
                    upper_price: 100.0,
                    long_exposure_units: 6.0,
                    short_exposure_units: 6.0,
                    notional_per_unit: 250.0,
                    min_rebalance_units: 0.5,
                    shape_family: poise_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                    max_notional: None,
                    daily_loss_limit: None,
                    stop_loss_pct: None,
                    tick_timeout_secs: None,
                },
            ],
            exchange: ExchangeConfig {
                ..Default::default()
            },
        };

        let error = assemble(&config).await.err().unwrap();
        assert!(error.to_string().contains("duplicate instrument"));
    }

    #[tokio::test]
    async fn assemble_requires_exchange_credentials_for_real_runtime() {
        let suffix = unique_test_environment();
        let config = Config {
            environment: suffix,
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                venue: Venue::Binance,
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
            exchange: ExchangeConfig {
                ..Default::default()
            },
        };

        let error = assemble(&config).await.err().unwrap();
        assert!(
            error
                .to_string()
                .contains("missing required exchange.api_key")
        );
    }

    #[test]
    fn real_runtime_rejects_test_environment() {
        let config = Config {
            environment: "test".into(),
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                venue: Venue::Binance,
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
            exchange: ExchangeConfig {
                api_key: Some("demo-key".into()),
                api_secret: Some("demo-secret".into()),
            },
        };

        let error = validate_exchange_runtime_config(&config).unwrap_err();
        assert!(error.to_string().contains("reserved for automated tests"));
    }

    #[test]
    fn testnet_runtime_uses_fixed_demo_endpoints() {
        let (rest_base_url, ws_base_url) = resolve_binance_endpoints("testnet").unwrap();
        assert_eq!(rest_base_url, "https://demo-fapi.binance.com");
        assert_eq!(ws_base_url, "wss://fstream.binancefuture.com");
    }

    #[tokio::test]
    async fn assemble_retries_transient_exchange_info_failures() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let exchange = Arc::new(FlakyExchangeInfoExchange::new(2));
        let config = Config {
            environment: unique_test_environment(),
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                venue: Venue::Binance,
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
            exchange: ExchangeConfig::default(),
        };

        let platform = super::assemble_with_components(
            &config,
            exchange.clone(),
            Arc::new(FakeMarketData::empty()),
            repository,
            Arc::new(SystemClock),
        )
        .await
        .unwrap();

        assert_eq!(
            exchange.get_exchange_info_calls.load(Ordering::SeqCst),
            3,
            "should retry until exchange info succeeds"
        );
        let manager = platform.state().write_service.manager();
        assert_eq!(manager.read().await.list_tracks().len(), 1);
    }

    #[tokio::test]
    async fn startup_margin_preflight_fails_when_configured_max_notional_exceeds_account_capacity()
    {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let exchange = Arc::new(LimitedMarginExchange {
            max_increase_notional: 500.0,
        });
        let config = Config {
            environment: unique_test_environment(),
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                venue: Venue::Binance,
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: poise_core::strategy::ShapeFamily::Linear,
                out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                max_notional: Some(20_000.0),
                daily_loss_limit: None,
                stop_loss_pct: None,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
        };

        let result = super::assemble_with_components(
            &config,
            exchange,
            Arc::new(FakeMarketData::empty()),
            repository,
            Arc::new(SystemClock),
        )
        .await;

        let error = match result {
            Ok(_) => panic!("assemble_with_components should reject insufficient account margin"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("insufficient account margin"));
    }

    #[test]
    fn test_runtime_reads_endpoints_from_env_lookup() {
        let (rest_base_url, ws_base_url) =
            resolve_binance_endpoints_with_lookup("test", |name| match name {
                "POISE_TEST_BINANCE_REST_BASE_URL" => Some("http://127.0.0.1:19080".into()),
                "POISE_TEST_BINANCE_WS_BASE_URL" => Some("ws://127.0.0.1:19081".into()),
                _ => None,
            })
            .unwrap();

        assert_eq!(rest_base_url, "http://127.0.0.1:19080");
        assert_eq!(ws_base_url, "ws://127.0.0.1:19081");
    }

    #[tokio::test]
    async fn start_market_data_tasks_broadcasts_events_to_ws_clients() {
        let (platform, btc_sender) = test_platform();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(platform.state());

        let handles = platform.start_market_data_tasks().await.unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (client, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let (_, mut stream) = client.split();

        btc_sender
            .send(PriceTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                reference_price: 95.0,
                mark_price: 95.0,
                timestamp: chrono::Utc::now(),
            })
            .await
            .unwrap();

        let message = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let payload: TrackStreamEvent = serde_json::from_str(message.to_text().unwrap()).unwrap();

        assert_eq!(payload.track_id, "btc-core");
        assert!(matches!(
            payload.payload,
            TrackStreamPayload::TrackListItemChanged { .. }
        ));

        server.abort();
        let _ = server.await;
        handles.market_task.abort();
        handles.user_task.abort();
        handles.effect_task.abort();
        handles.recovery_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
        let _ = handles.effect_task.await;
        let _ = handles.recovery_task.await;
    }

    #[tokio::test]
    async fn pause_command_persists_across_reassembly() {
        let suffix = unique_test_environment();
        let config = Config {
            environment: suffix.clone(),
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                venue: Venue::Binance,
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
            exchange: ExchangeConfig {
                ..Default::default()
            },
        };

        let first = assemble_with_fake_ports(&config).await.unwrap();
        let app = router(first.state());
        let pause = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/tracks/btc-core/commands")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::to_vec(&serde_json::json!({ "command": "pause" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(pause.status(), axum::http::StatusCode::OK);

        let second = assemble_with_fake_ports(&config).await.unwrap();
        let manager_handle = second.state().write_service.manager();
        let manager = manager_handle.read().await;
        let grid = manager.get_track("btc-core").unwrap();

        assert_eq!(grid.status(), &poise_engine::runtime::TrackStatus::Paused);

        let _ = std::fs::remove_dir_all(std::path::Path::new(".data").join(&suffix));
    }

    #[tokio::test]
    async fn newer_tick_snapshot_is_not_overwritten_by_older_command_snapshot() {
        let persistence = Arc::new(BlockingPersistence::default());
        let (platform, _btc_sender) = test_platform_with_repository(persistence.clone());
        let app = router(platform.state());

        {
            let manager_handle = platform.state.write_service.manager();
            let mut manager = manager_handle.write().await;
            let tick = PriceTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                reference_price: 95.0,
                mark_price: 95.0,
                timestamp: chrono::Utc::now(),
            };
            let _ = manager.observe(
                &TrackId::new("btc-core"),
                TrackObservation::Market(MarketObservation {
                    reference_price: tick.reference_price,
                }),
            );
            manager.pause_track("btc-core").unwrap();
        }

        let resume_request = tokio::spawn(async move {
            tower::ServiceExt::oneshot(
                app,
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/tracks/btc-core/commands")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({ "command": "resume" })).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
        });

        persistence.wait_for_first_save_start().await;

        let tick_state = platform.state.clone();
        let tick_request = tokio::spawn(async move {
            let tick = PriceTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                reference_price: 85.0,
                mark_price: 85.0,
                timestamp: chrono::Utc::now(),
            };
            tick_state
                .write_service
                .observe_market("btc-core", tick.reference_price)
                .await
                .map(|_| ())
        });

        let second_save_started = tokio::time::timeout(
            Duration::from_millis(100),
            persistence.wait_for_started_saves(2),
        )
        .await;
        persistence.release_first_save();

        let response = resume_request.await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        persistence.wait_for_completed_saves(2).await;
        let _ = tick_request.await.unwrap();
        assert!(
            second_save_started.is_err(),
            "tick save should wait for command save to finish"
        );

        let snapshot = persistence
            .load_track_state("btc-core")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.status, poise_engine::runtime::TrackStatus::Frozen);
        assert_eq!(snapshot.observed.reference_price, Some(85.0));
    }

    fn test_platform() -> (ServerPlatform, mpsc::Sender<PriceTick>) {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        test_platform_with_repository(storage)
    }

    async fn assemble_with_fake_ports(config: &Config) -> Result<ServerPlatform> {
        let db_path = config.default_db_path();
        super::ensure_parent_dir(&db_path)?;
        let repository = Arc::new(SqliteStorage::new(&db_path)?);
        super::assemble_with_components(
            config,
            Arc::new(FakeExchange),
            Arc::new(FakeMarketData::empty()),
            repository,
            Arc::new(SystemClock),
        )
        .await
    }

    fn test_platform_with_repository<R>(
        repository: Arc<R>,
    ) -> (ServerPlatform, mpsc::Sender<PriceTick>)
    where
        R: StateRepositoryPort + TrackReadRepositoryPort + 'static,
    {
        let (btc_sender, btc_receiver) = mpsc::channel(8);
        let mut receivers = HashMap::new();
        receivers.insert("BTCUSDT".to_string(), btc_receiver);
        let exchange = Arc::new(FakeExchange);
        let market_data = Arc::new(FakeMarketData {
            receivers: Mutex::new(receivers),
        });

        let mut manager = TrackManager::new(Arc::new(SystemClock));
        manager
            .add_track(
                TrackId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                poise_core::strategy::TrackConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: 0.5,
                    shape_family: poise_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                },
                poise_core::risk::CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: -100.0,
                    stop_loss_pct: 10.0,
                },
                test_exchange_rules(),
            )
            .unwrap();
        let (events, _) = broadcast::channel(16);
        let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
        let read_repository: Arc<dyn TrackReadRepositoryPort> = repository;
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            state_repository.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            state_repository,
            Arc::new(TrackQueryService::new(read_repository)),
            Arc::new(TrackProjector::new()),
        );

        (
            ServerPlatform {
                state: state.clone(),
                runtime: crate::runtime::ServerRuntime::new(state, exchange, market_data),
            },
            btc_sender,
        )
    }

    struct FakeExchange;

    struct LimitedMarginExchange {
        max_increase_notional: f64,
    }

    struct FlakyExchangeInfoExchange {
        remaining_failures: AtomicUsize,
        get_exchange_info_calls: AtomicUsize,
    }

    impl FlakyExchangeInfoExchange {
        fn new(initial_failures: usize) -> Self {
            Self {
                remaining_failures: AtomicUsize::new(initial_failures),
                get_exchange_info_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Err(anyhow!("not used in tests"))
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Err(anyhow!("not used in tests"))
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
                rules: test_exchange_rules(),
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
                observed_at: chrono::Utc::now(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Ok(chrono::Utc::now())
        }
    }

    #[async_trait::async_trait]
    impl ExchangePort for FlakyExchangeInfoExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Err(anyhow!("not used in tests"))
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            self.get_exchange_info_calls.fetch_add(1, Ordering::SeqCst);
            let remaining = self.remaining_failures.load(Ordering::SeqCst);
            if remaining > 0 {
                self.remaining_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow!("temporary exchange info timeout"));
            }

            Ok(ExchangeInfo {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                rules: test_exchange_rules(),
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
                observed_at: chrono::Utc::now(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Err(anyhow!("not used in tests"))
        }
    }

    #[async_trait::async_trait]
    impl ExchangePort for LimitedMarginExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Err(anyhow!("not used in tests"))
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                rules: test_exchange_rules(),
            })
        }

        async fn get_account_margin_snapshot(
            &self,
            instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountMarginSnapshot> {
            Ok(poise_engine::ports::AccountMarginSnapshot {
                venue: instrument.venue,
                available_balance: self.max_increase_notional,
                total_wallet_balance: self.max_increase_notional,
                max_increase_notional: self.max_increase_notional,
                observed_at: chrono::Utc::now(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Err(anyhow!("not used in tests"))
        }
    }

    #[derive(Default)]
    struct BlockingPersistence {
        snapshots: AsyncMutex<HashMap<String, TrackSnapshot>>,
        started_saves: AtomicUsize,
        completed_saves: AtomicUsize,
        first_save_started: Notify,
        first_save_release: Notify,
        completed_save: Notify,
    }

    impl BlockingPersistence {
        async fn wait_for_first_save_start(&self) {
            while self.started_saves.load(Ordering::SeqCst) == 0 {
                self.first_save_started.notified().await;
            }
        }

        async fn wait_for_started_saves(&self, expected: usize) {
            while self.started_saves.load(Ordering::SeqCst) < expected {
                self.first_save_started.notified().await;
            }
        }

        fn release_first_save(&self) {
            self.first_save_release.notify_waiters();
        }

        async fn wait_for_completed_saves(&self, expected: usize) {
            while self.completed_saves.load(Ordering::SeqCst) < expected {
                self.completed_save.notified().await;
            }
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for BlockingPersistence {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &TrackSnapshot,
            _events: &[EngineDomainEvent],
            _effects: &[poise_engine::transition::TrackEffect],
            _effect_status_update: Option<&poise_engine::ports::EffectStatusUpdate>,
        ) -> Result<poise_engine::ports::CommittedTrackWrite> {
            let save_index = self.started_saves.fetch_add(1, Ordering::SeqCst);
            self.first_save_started.notify_waiters();
            if save_index == 0 {
                self.first_save_release.notified().await;
            }

            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());
            self.completed_saves.fetch_add(1, Ordering::SeqCst);
            self.completed_save.notify_waiters();
            Ok(poise_engine::ports::CommittedTrackWrite {
                track_id: poise_engine::track::TrackId::new(id),
                effects: Vec::new(),
            })
        }

        async fn load_track_state(&self, id: &str) -> Result<Option<TrackSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_track_events(&self, _id: &str) -> Result<Vec<EngineDomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_dispatchable_effects(
            &self,
        ) -> Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            _track_id: &TrackId,
        ) -> Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_track_batch(
            &self,
            _track_id: &TrackId,
            _batch_id: &str,
        ) -> Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn save_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &poise_engine::ports::FollowUpRetirementRequest,
        ) -> Result<()> {
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            _track_id: &TrackId,
        ) -> Result<Vec<poise_engine::ports::FollowUpRetirementRequest>> {
            Ok(Vec::new())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &poise_engine::ports::FollowUpRetirementRequest,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for BlockingPersistence {
        async fn list_track_snapshots(
            &self,
        ) -> Result<Vec<poise_engine::ports::StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .values()
                .cloned()
                .map(|snapshot| poise_engine::ports::StoredTrackSnapshot {
                    snapshot,
                    updated_at: chrono::Utc::now(),
                })
                .collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<poise_engine::ports::StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .get(track_id.as_str())
                .cloned()
                .map(|snapshot| poise_engine::ports::StoredTrackSnapshot {
                    snapshot,
                    updated_at: chrono::Utc::now(),
                }))
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<poise_engine::ports::StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<poise_engine::ports::PersistedTrackEffect>> {
            Ok(Vec::new())
        }
    }

    struct FakeMarketData {
        receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
    }

    impl FakeMarketData {
        fn empty() -> Self {
            Self {
                receivers: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::MarketDataPort for FakeMarketData {
        async fn subscribe_prices(
            &self,
            instrument: &Instrument,
        ) -> Result<mpsc::Receiver<PriceTick>> {
            self.receivers
                .lock()
                .unwrap()
                .remove(&instrument.symbol)
                .ok_or_else(|| anyhow!("no test receiver for symbol `{}`", instrument.symbol))
        }

        async fn subscribe_user_data(
            &self,
        ) -> Result<mpsc::Receiver<poise_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
