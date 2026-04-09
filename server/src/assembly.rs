use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::path::Path;
use std::sync::Arc;

#[cfg(test)]
use anyhow::Context;
use anyhow::{Result, anyhow};
use chrono::Utc;
use poise_application::{
    AccountMonitor, ApplicationNotification, PreparedTrackRegistry, TrackCommandService,
    TrackDebugQueryService, TrackEffectService, TrackEffectStore, TrackMutationStore,
    TrackObservationService, TrackQueryService, TrackServiceSet,
};
use poise_binance::connect as connect_binance;
use poise_engine::manager::TrackManager;
use poise_engine::ports::{AccountPort, ClockPort, MetadataPort};
#[cfg(test)]
use poise_engine::ports::{AccountSummaryPort, ExecutionPort, MarketDataPort};
use poise_engine::track::{Instrument, TrackId, Venue};
#[cfg(test)]
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tokio::time::{Duration, sleep};

use crate::account_projector::AccountProjector;
use crate::config::{Config, ExchangeConfig};
use crate::exchange::Exchange;
use crate::exchange_freshness::ExchangeFreshness;
use crate::projector::TrackProjector;
use crate::runtime::{
    AccountMarginGuardStore, RuntimeHandles, RuntimePorts, ServerRuntime, TrackReconcileGuards,
};
use crate::server_context::{
    EffectWorkerState, HttpState, ReconcileState, RuntimeState, WebSocketState,
};
use crate::state_bootstrap::StateRepositories;
use crate::submit_preflight::SubmitPreflight;
#[cfg(test)]
use crate::test_support::build_test_contexts_from_runtime_states;
#[cfg(test)]
use crate::test_support::{EffectWorkerTestContext, RuntimeTestContext};

pub struct ServerPlatform {
    http_state: HttpState,
    websocket_state: WebSocketState,
    #[cfg(test)]
    manager: Arc<RwLock<TrackManager>>,
    #[cfg(test)]
    runtime_test_context: RuntimeTestContext,
    #[cfg(test)]
    effect_worker_test_context: EffectWorkerTestContext,
    pub runtime: ServerRuntime,
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
            return Err(anyhow!("duplicate track id `{}`", track_id.as_str()));
        }
    }
    Ok(())
}

pub(crate) async fn build_exchange(config: &ExchangeConfig) -> Result<Exchange> {
    match config {
        ExchangeConfig::Binance(binance_config) => {
            let connected = connect_binance(binance_config).await?;
            Ok(Exchange::new(
                Venue::Binance,
                connected.execution(),
                connected.market_data(),
                connected.account_summary(),
                connected.account(),
                connected.metadata(),
            ))
        }
    }
}

pub async fn assemble(
    config: &Config,
    prepared_registry: Arc<PreparedTrackRegistry>,
    repositories: StateRepositories,
) -> Result<ServerPlatform> {
    validate_unique_instruments(
        prepared_registry
            .iter()
            .map(|track| track.instrument().clone()),
    )?;
    validate_unique_track_ids(
        prepared_registry
            .iter()
            .map(|track| track.track_id().clone()),
    )?;
    let exchange = build_exchange(&config.exchange).await?;
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock);

    assemble_with_state_store(config, prepared_registry, exchange, repositories, clock).await
}

#[cfg(test)]
pub(crate) struct ExchangePorts {
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
}

#[cfg(test)]
impl ExchangePorts {
    pub(crate) fn new(
        execution: Arc<dyn ExecutionPort>,
        market_data: Arc<dyn MarketDataPort>,
        account_summary: Arc<dyn AccountSummaryPort>,
        account: Arc<dyn AccountPort>,
        metadata: Arc<dyn MetadataPort>,
    ) -> Self {
        Self {
            execution,
            market_data,
            account_summary,
            account,
            metadata,
        }
    }
}

#[cfg(test)]
pub(crate) async fn assemble_with_exchange_ports(
    config: &Config,
    prepared_registry: Arc<PreparedTrackRegistry>,
    exchange_ports: ExchangePorts,
    repositories: StateRepositories,
    clock: Arc<dyn ClockPort>,
) -> Result<ServerPlatform> {
    let exchange = Exchange::new(
        config.exchange.venue(),
        exchange_ports.execution,
        exchange_ports.market_data,
        exchange_ports.account_summary,
        exchange_ports.account,
        exchange_ports.metadata,
    );
    assemble_with_state_store(config, prepared_registry, exchange, repositories, clock).await
}

async fn assemble_with_state_store(
    config: &Config,
    prepared_registry: Arc<PreparedTrackRegistry>,
    exchange: Exchange,
    repositories: StateRepositories,
    clock: Arc<dyn ClockPort>,
) -> Result<ServerPlatform> {
    validate_unique_instruments(
        prepared_registry
            .iter()
            .map(|track| track.instrument().clone()),
    )?;
    validate_unique_track_ids(
        prepared_registry
            .iter()
            .map(|track| track.track_id().clone()),
    )?;

    let mut manager = TrackManager::new(clock);
    let mut account_capacity_snapshots = HashMap::new();
    for track in prepared_registry.iter() {
        let track_id = track.track_id().clone();
        let instrument = track.instrument().clone();
        let info = load_exchange_info_with_retry(exchange.metadata(), &instrument).await?;
        let account_capacity_snapshot =
            load_account_capacity_snapshot_with_retry(exchange.account(), &instrument).await?;
        if track.budget().max_notional > account_capacity_snapshot.max_increase_notional {
            return Err(anyhow!(
                "insufficient account margin for configured max_notional on track `{}`: required {}, available {}",
                track_id.as_str(),
                track.budget().max_notional,
                account_capacity_snapshot.max_increase_notional
            ));
        }
        account_capacity_snapshots.insert(instrument.clone(), account_capacity_snapshot);
        manager.add_track_with_tick_timeout_secs(
            track_id.clone(),
            instrument,
            track.track_config().clone(),
            track.budget(),
            info.rules,
            track.tick_timeout_secs(),
        )?;
        if let Some(snapshot) = repositories.load_track_state(track_id.as_str()).await? {
            manager.restore_track_state(&snapshot)?;
        }
    }

    let (notifications, _) = broadcast::channel(256);
    let mutation_store = repositories.mutation_store();
    let query_store = repositories.query_store();
    let effect_store = repositories.effect_store();
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let write_services = TrackServiceSet::new(
        manager,
        mutation_store.clone(),
        effect_store.clone(),
        notifications.clone(),
        account_margin_guard.clone(),
    );
    let command_service = Arc::new(write_services.command);
    let observation_service = Arc::new(write_services.observation);
    let effect_service = Arc::new(write_services.effect);
    #[cfg(test)]
    let manager = observation_service.manager();
    let query_service = Arc::new(TrackQueryService::new(
        query_store,
        prepared_registry.clone(),
    ));
    let debug_query_service = Arc::new(TrackDebugQueryService::new(query_service.clone()));
    let projector = Arc::new(TrackProjector::new());
    let account_projector = Arc::new(AccountProjector::new());
    let account_monitor = if let Some(account_store) = repositories.account_monitor_store() {
        Arc::new(
            AccountMonitor::restore(
                exchange.account_summary_port(),
                account_store,
                notifications.clone(),
                config.account_monitor.clone(),
            )
            .await?,
        )
    } else {
        Arc::new(AccountMonitor::unavailable(
            notifications.clone(),
            config.account_monitor.clone(),
        ))
    };
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let reconcile_guards = Arc::new(TrackReconcileGuards::default());
    let submit_preflight = Arc::new(SubmitPreflight::new());
    let reconcile_state = build_reconcile_state(
        observation_service.clone(),
        mutation_store.clone(),
        effect_store.clone(),
        exchange_freshness.clone(),
        reconcile_guards.clone(),
        submit_preflight.clone(),
    );
    let http_state = build_http_state(
        command_service.clone(),
        query_service.clone(),
        debug_query_service.clone(),
        projector.clone(),
        account_monitor.clone(),
        account_projector.clone(),
    );
    let websocket_state = build_websocket_state(
        notifications.clone(),
        query_service.clone(),
        projector.clone(),
        account_monitor.clone(),
        account_projector.clone(),
    );
    let runtime_state = build_runtime_state(
        reconcile_state.clone(),
        notifications.clone(),
        account_monitor.clone(),
        account_margin_guard.clone(),
    );
    let effect_worker_state = build_effect_worker_state(
        reconcile_state.clone(),
        effect_service.clone(),
        account_margin_guard.clone(),
    );
    #[cfg(test)]
    let (runtime_test_context, effect_worker_test_context) =
        build_test_contexts_from_runtime_states(
            runtime_state.clone(),
            effect_worker_state.clone(),
            manager.clone(),
            notifications.clone(),
            projector.clone(),
            command_service.clone(),
            observation_service.clone(),
        );

    Ok(ServerPlatform {
        http_state,
        websocket_state,
        #[cfg(test)]
        manager,
        #[cfg(test)]
        runtime_test_context: runtime_test_context.clone(),
        #[cfg(test)]
        effect_worker_test_context: effect_worker_test_context.clone(),
        runtime: ServerRuntime::with_account_capacity_snapshots(
            runtime_state,
            effect_worker_state,
            RuntimePorts::new(
                exchange.execution_port(),
                exchange.market_data_port(),
                exchange.account_port(),
                exchange.metadata_port(),
            ),
            account_capacity_snapshots,
            Duration::from_secs(1),
        ),
    })
}

async fn load_exchange_info_with_retry(
    metadata: &dyn MetadataPort,
    instrument: &Instrument,
) -> Result<poise_engine::ports::ExchangeInfo> {
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match metadata.get_exchange_info(instrument).await {
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

async fn load_account_capacity_snapshot_with_retry(
    account: &dyn AccountPort,
    instrument: &Instrument,
) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match account.get_account_capacity_snapshot(instrument).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => {
                if attempt + 1 == STARTUP_RETRY_ATTEMPTS {
                    return Err(error);
                }
                tracing::warn!(
                    instrument = %instrument.symbol,
                    attempt = attempt + 1,
                    max_attempts = STARTUP_RETRY_ATTEMPTS,
                    "startup account capacity probe failed: {error}"
                );
                last_error = Some(error);
            }
        }

        sleep(STARTUP_RETRY_DELAY).await;
    }

    Err(last_error.unwrap_or_else(|| anyhow!("failed to load account capacity snapshot")))
}

impl ServerPlatform {
    pub fn http_state(&self) -> HttpState {
        self.http_state.clone()
    }

    pub fn websocket_state(&self) -> WebSocketState {
        self.websocket_state.clone()
    }

    #[cfg(test)]
    pub fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.manager.clone()
    }

    #[cfg(test)]
    pub fn runtime_test_context(&self) -> RuntimeTestContext {
        self.runtime_test_context.clone()
    }

    #[cfg(test)]
    pub fn effect_worker_test_context(&self) -> crate::test_support::EffectWorkerTestContext {
        self.effect_worker_test_context.clone()
    }

    #[cfg(test)]
    pub fn exchange_freshness(&self) -> Arc<ExchangeFreshness> {
        self.runtime_test_context.exchange_freshness.clone()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn start_market_data_tasks(&self) -> Result<RuntimeHandles> {
        self.runtime.start().await
    }
}

#[cfg(test)]
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

pub(crate) fn build_http_state(
    command_service: Arc<TrackCommandService>,
    query_service: Arc<TrackQueryService>,
    debug_query_service: Arc<TrackDebugQueryService>,
    projector: Arc<TrackProjector>,
    account_monitor: Arc<AccountMonitor>,
    account_projector: Arc<AccountProjector>,
) -> HttpState {
    HttpState {
        command_service,
        query_service,
        debug_query_service,
        projector,
        account_monitor,
        account_projector,
    }
}

pub(crate) fn build_websocket_state(
    notifications: broadcast::Sender<ApplicationNotification>,
    query_service: Arc<TrackQueryService>,
    projector: Arc<TrackProjector>,
    account_monitor: Arc<AccountMonitor>,
    account_projector: Arc<AccountProjector>,
) -> WebSocketState {
    WebSocketState {
        notifications,
        query_service,
        projector,
        account_monitor,
        account_projector,
    }
}

pub(crate) fn build_reconcile_state(
    observation_service: Arc<TrackObservationService>,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_store: Arc<dyn TrackEffectStore>,
    exchange_freshness: Arc<ExchangeFreshness>,
    reconcile_guards: Arc<TrackReconcileGuards>,
    submit_preflight: Arc<SubmitPreflight>,
) -> ReconcileState {
    ReconcileState {
        observation_service,
        mutation_store,
        effect_store,
        exchange_freshness,
        reconcile_guards,
        submit_preflight,
    }
}

pub(crate) fn build_runtime_state(
    reconcile: ReconcileState,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_monitor: Arc<AccountMonitor>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
) -> RuntimeState {
    RuntimeState {
        reconcile,
        notifications,
        account_monitor,
        account_margin_guard,
    }
}

pub(crate) fn build_effect_worker_state(
    reconcile: ReconcileState,
    effect_service: Arc<TrackEffectService>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
) -> EffectWorkerState {
    EffectWorkerState {
        reconcile,
        effect_service,
        account_margin_guard,
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
    use poise_application::{
        CommittedTrackWrite, EffectStatusUpdate, FollowUpRetirementRequest, PersistedTrackEffect,
        StoredTrackEvent, StoredTrackSnapshot, TrackEffectStore, TrackMutationStore,
        TrackQueryStore,
    };
    use poise_core::events::DomainEvent as EngineDomainEvent;
    use poise_engine::manager::TrackManager;
    use poise_engine::observation::{MarketObservation, TrackObservation};
    use poise_engine::ports::{
        AccountPort, AccountSummaryPort, ExchangeInfo, ExchangeOrder, ExecutionPort,
        MarketDataPort, MetadataPort, OrderReceipt, OrderRequest, Position, PriceTick,
    };
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_protocol::StreamEvent;
    use poise_storage::sqlite::SqliteStorage;
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};

    use crate::runtime::AccountMarginGuardStore;
    use tokio_tungstenite::connect_async;

    use crate::config::{Config, ExchangeConfig, TrackDefinition, parse_config};
    use crate::http::router;
    use crate::projector::TrackProjector;
    use crate::state_bootstrap::StateRepositories;
    use crate::test_support::{
        build_http_state as build_test_http_state, build_runtime_and_effect_worker_test_contexts,
        build_test_application_services, build_websocket_state as build_test_websocket_state,
        unavailable_account_monitor,
    };
    use poise_application::{
        ConfiguredTrackDefinition, PreparedTrackRegistry, TrackDebugQueryService, TrackQueryService,
    };

    use super::{
        ServerPlatform, SystemClock, assemble, build_exchange, validate_unique_instruments,
        validate_unique_track_ids,
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

    fn test_prepared_registry(config: &Config) -> Arc<PreparedTrackRegistry> {
        let configured = config
            .tracks
            .iter()
            .map(|track| {
                ConfiguredTrackDefinition::try_from_input(
                    track.to_configured_input(config.exchange.venue()),
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        Arc::new(PreparedTrackRegistry::new(configured).unwrap())
    }

    #[test]
    fn track_instrument_uses_service_exchange_venue() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"
deployment = "testnet"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        let instrument = test_prepared_registry(&config)
            .get(&TrackId::new("btc-core"))
            .unwrap()
            .instrument()
            .clone();

        assert_eq!(instrument.venue, Venue::Binance);
        assert_eq!(instrument.symbol, "BTCUSDT");
    }

    #[tokio::test]
    async fn assemble_accepts_distinct_execution_account_summary_account_metadata_and_market_ports()
    {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"
deployment = "testnet"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());

        let platform = super::assemble_with_exchange_ports(
            &config,
            test_prepared_registry(&config),
            super::ExchangePorts::new(
                Arc::new(FakeExecutionPort),
                Arc::new(FakeMarketDataPort),
                Arc::new(FakeAccountSummaryPort),
                Arc::new(FakeAccountPort),
                Arc::new(FakeMetadataPort),
            ),
            StateRepositories::new(repository),
            Arc::new(SystemClock),
        )
        .await
        .unwrap();

        assert_eq!(platform.manager().read().await.list_tracks().len(), 1);
    }

    #[tokio::test]
    async fn build_exchange_uses_exchange_deployment_for_binance_endpoint_selection() {
        let config = parse_config(
            r#"
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
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        let exchange = build_exchange(&config.exchange).await.unwrap();

        assert_eq!(exchange.venue(), Venue::Binance);
    }

    #[tokio::test]
    async fn assemble_accepts_prepared_state_store_instead_of_bootstrap_flag() {
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let repositories = StateRepositories::new(repository);

        let error = assemble(&config, test_prepared_registry(&config), repositories)
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("missing required exchange.api_key")
        );
    }

    #[tokio::test]
    async fn assembles_platform_with_all_instances_registered() {
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![
                TrackDefinition {
                    track_id: "btc-core".into(),
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                    out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
                    max_notional: None,
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                    tick_timeout_secs: None,
                },
                TrackDefinition {
                    track_id: "eth-core".into(),
                    symbol: "ETHUSDT".into(),
                    lower_price: 2000.0,
                    upper_price: 2500.0,
                    long_exposure_units: 5.0,
                    short_exposure_units: 3.0,
                    notional_per_unit: 500.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                    out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
                    max_notional: None,
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                    tick_timeout_secs: None,
                },
            ],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };

        let instance_dir = tempfile::tempdir().unwrap();
        let platform = assemble_with_fake_ports(&config, instance_dir.path())
            .await
            .unwrap();
        let manager_handle = platform.manager();
        let manager = manager_handle.read().await;

        assert_eq!(manager.list_tracks().len(), 2);
        let track = manager.get_track("btc-core").unwrap();
        assert_eq!(track.budget().max_notional, 3000.0);
        assert!(
            crate::instance_dir::InstanceDir::new(instance_dir.path())
                .db_path()
                .exists()
        );
    }

    #[test]
    fn assemble_rejects_duplicate_track_ids() {
        let error =
            validate_unique_track_ids([TrackId::new("alpha"), TrackId::new("alpha")]).unwrap_err();
        assert!(error.to_string().contains("duplicate track id"));
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
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![
                TrackDefinition {
                    track_id: "btc-core".into(),
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                    out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
                    max_notional: None,
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                    tick_timeout_secs: None,
                },
                TrackDefinition {
                    track_id: "btc-alt".into(),
                    symbol: "BTCUSDT".into(),
                    lower_price: 80.0,
                    upper_price: 100.0,
                    long_exposure_units: 6.0,
                    short_exposure_units: 6.0,
                    notional_per_unit: 250.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                    out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
                    max_notional: None,
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                    tick_timeout_secs: None,
                },
            ],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };

        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let repositories = StateRepositories::new(repository);
        let error = assemble(&config, test_prepared_registry(&config), repositories)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("duplicate instrument"));
    }

    #[tokio::test]
    async fn assemble_requires_exchange_credentials_for_real_runtime() {
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
                max_notional: None,
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };

        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let repositories = StateRepositories::new(repository);
        let error = assemble(&config, test_prepared_registry(&config), repositories)
            .await
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("missing required exchange.api_key")
        );
    }

    #[tokio::test]
    async fn assemble_retries_transient_exchange_info_failures() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let exchange = Arc::new(FlakyExchangeInfoExchange::new(2));
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
                max_notional: None,
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };

        let platform = super::assemble_with_exchange_ports(
            &config,
            test_prepared_registry(&config),
            super::ExchangePorts::new(
                exchange.clone(),
                Arc::new(FakeMarketData::empty()),
                exchange.clone(),
                exchange.clone(),
                exchange.clone(),
            ),
            StateRepositories::new(repository),
            Arc::new(SystemClock),
        )
        .await
        .unwrap();

        assert_eq!(
            exchange.get_exchange_info_calls.load(Ordering::SeqCst),
            3,
            "should retry until exchange info succeeds"
        );
        let manager = platform.manager();
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
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
                max_notional: Some(20_000.0),
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };

        let result = super::assemble_with_exchange_ports(
            &config,
            test_prepared_registry(&config),
            super::ExchangePorts::new(
                exchange.clone(),
                Arc::new(FakeMarketData::empty()),
                exchange.clone(),
                exchange.clone(),
                exchange,
            ),
            StateRepositories::new(repository),
            Arc::new(SystemClock),
        )
        .await;

        let error = match result {
            Ok(_) => {
                panic!("assemble_with_exchange_ports should reject insufficient account margin")
            }
            Err(error) => error,
        };

        assert!(error.to_string().contains("insufficient account margin"));
    }

    #[tokio::test]
    async fn start_market_data_tasks_broadcasts_events_to_ws_clients() {
        let (platform, btc_sender) = test_platform();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(platform.http_state(), platform.websocket_state());

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
        let payload: StreamEvent = serde_json::from_str(message.to_text().unwrap()).unwrap();
        assert!(matches!(
            payload,
            StreamEvent::TrackListItemChanged { ref track_id, .. } if track_id == "btc-core"
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
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                out_of_band_policy: Some(poise_core::strategy::OutOfBandPolicy::Freeze),
                max_notional: None,
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };

        let instance_dir = tempfile::tempdir().unwrap();
        let first = assemble_with_fake_ports(&config, instance_dir.path())
            .await
            .unwrap();
        let app = router(first.http_state(), first.websocket_state());
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

        let second = assemble_with_fake_ports(&config, instance_dir.path())
            .await
            .unwrap();
        let manager_handle = second.manager();
        let manager = manager_handle.read().await;
        let track = manager.get_track("btc-core").unwrap();

        assert_eq!(track.status(), &poise_engine::runtime::TrackStatus::Paused);
    }

    #[tokio::test]
    async fn runtime_state_exposes_observation_and_account_paths_only() {
        let (platform, _) = test_platform();
        let state = platform.runtime_test_context();

        assert_eq!(state.track_instruments().await.len(), 1);
        assert!(
            state
                .runtime_state()
                .account_monitor
                .current_summary()
                .await
                .is_none()
        );
        let _receiver = state.notifications.subscribe();
    }

    #[tokio::test]
    async fn effect_worker_state_exposes_effect_execution_paths_only() {
        let (platform, _) = test_platform();
        let state = platform.effect_worker_test_context().effect_worker_state;

        assert!(
            state
                .reconcile
                .effect_store
                .list_dispatchable_effects()
                .await
                .unwrap()
                .is_empty()
        );
        let _ = state
            .account_margin_guard
            .constraint_for(&Instrument::new(Venue::Binance, "BTCUSDT"));
    }

    #[tokio::test]
    async fn newer_tick_snapshot_is_not_overwritten_by_older_command_snapshot() {
        let persistence = Arc::new(BlockingPersistence::default());
        let (platform, _btc_sender) = test_platform_with_repository(persistence.clone());
        let app = router(platform.http_state(), platform.websocket_state());

        {
            let manager_handle = platform.manager();
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

        let tick_state = platform.runtime_test_context();
        let tick_request = tokio::spawn(async move {
            let tick = PriceTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                reference_price: 85.0,
                mark_price: 85.0,
                timestamp: chrono::Utc::now(),
            };
            tick_state
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

    #[tokio::test]
    async fn build_test_context_initializes_fresh_exchange_freshness_store() {
        let (platform, _) = test_platform();

        assert!(!platform.exchange_freshness().is_stale("btc-core").await);
    }

    #[tokio::test]
    async fn build_test_context_reuses_runtime_coordination_objects() {
        let (platform, _) = test_platform();
        let runtime_context = platform.runtime_test_context();
        let effect_worker_context = platform.effect_worker_test_context();

        assert!(Arc::ptr_eq(
            &runtime_context.exchange_freshness,
            &effect_worker_context.exchange_freshness,
        ));
        assert!(Arc::ptr_eq(
            &runtime_context.submit_preflight,
            &effect_worker_context.submit_preflight,
        ));
    }

    fn test_platform() -> (ServerPlatform, mpsc::Sender<PriceTick>) {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        test_platform_with_repository(storage)
    }

    async fn assemble_with_fake_ports(
        config: &Config,
        instance_dir: &std::path::Path,
    ) -> Result<ServerPlatform> {
        let db_path = crate::instance_dir::InstanceDir::new(instance_dir).db_path();
        super::ensure_parent_dir(&db_path)?;
        let repository = Arc::new(SqliteStorage::new(&db_path)?);
        let exchange = Arc::new(FakeExchange);
        super::assemble_with_exchange_ports(
            config,
            test_prepared_registry(config),
            super::ExchangePorts::new(
                exchange.clone(),
                Arc::new(FakeMarketData::empty()),
                exchange.clone(),
                exchange.clone(),
                exchange,
            ),
            StateRepositories::new(repository),
            Arc::new(SystemClock),
        )
        .await
    }

    fn test_platform_with_repository<R>(
        repository: Arc<R>,
    ) -> (ServerPlatform, mpsc::Sender<PriceTick>)
    where
        R: TrackMutationStore + TrackEffectStore + TrackQueryStore + 'static,
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
                    daily_loss_limit: 100.0,
                    total_loss_limit: 300.0,
                },
                test_exchange_rules(),
            )
            .unwrap();
        let (events, _) = broadcast::channel(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectStore> = repository.clone();
        let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store.clone(),
            effect_store.clone(),
            events.clone(),
            account_margin_guard.clone(),
        );
        let query_service = Arc::new(TrackQueryService::new(
            repository.clone() as Arc<dyn TrackQueryStore>,
            crate::test_support::test_prepared_registry("btc-core"),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(query_service.clone()));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(events.clone());
        let account_projector = Arc::new(crate::account_projector::AccountProjector::new());
        let (runtime_test_context, effect_worker_test_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                mutation_store,
                effect_store,
                account_monitor.clone(),
                projector.clone(),
            );

        (
            ServerPlatform {
                http_state: build_test_http_state(
                    &services,
                    query_service.clone(),
                    debug_query_service,
                    projector.clone(),
                    account_monitor.clone(),
                    account_projector.clone(),
                ),
                websocket_state: build_test_websocket_state(
                    &services,
                    query_service,
                    projector,
                    account_monitor,
                    account_projector,
                ),
                manager: services.observation_service.manager(),
                runtime_test_context: runtime_test_context.clone(),
                effect_worker_test_context: effect_worker_test_context.clone(),
                runtime: crate::runtime::ServerRuntime::new(
                    runtime_test_context.runtime_state(),
                    effect_worker_test_context.effect_worker_state.clone(),
                    crate::runtime::RuntimePorts::new(
                        exchange.clone(),
                        market_data,
                        exchange.clone(),
                        exchange,
                    ),
                ),
            },
            btc_sender,
        )
    }

    struct FakeExchange;
    #[derive(Default)]
    struct FakeExecutionPort;
    #[derive(Default)]
    struct FakeAccountSummaryPort;
    #[derive(Default)]
    struct FakeAccountPort;
    #[derive(Default)]
    struct FakeMetadataPort;
    #[derive(Default)]
    struct FakeMarketDataPort;

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
    impl poise_engine::ports::AccountSummaryPort for FakeExchange {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: chrono::Utc::now(),
            })
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for FakeExchange {
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
                rules: test_exchange_rules(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Ok(chrono::Utc::now())
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountSummaryPort for FlakyExchangeInfoExchange {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: chrono::Utc::now(),
            })
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for FlakyExchangeInfoExchange {
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
    }

    #[async_trait::async_trait]
    impl AccountPort for FlakyExchangeInfoExchange {
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
    impl MetadataPort for FlakyExchangeInfoExchange {
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

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Err(anyhow!("not used in tests"))
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountSummaryPort for LimitedMarginExchange {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: self.max_increase_notional,
                available: self.max_increase_notional,
                unrealized_pnl: 0.0,
                observed_at: chrono::Utc::now(),
            })
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for LimitedMarginExchange {
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
    }

    #[async_trait::async_trait]
    impl AccountPort for LimitedMarginExchange {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
            Ok(poise_engine::ports::AccountCapacitySnapshot {
                max_increase_notional: self.max_increase_notional,
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
    impl MetadataPort for LimitedMarginExchange {
        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                rules: test_exchange_rules(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Err(anyhow!("not used in tests"))
        }
    }

    #[derive(Default)]
    struct BlockingPersistence {
        snapshots: AsyncMutex<HashMap<String, poise_engine::snapshot::TrackRuntimeSnapshot>>,
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
    impl TrackMutationStore for BlockingPersistence {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &poise_engine::snapshot::TrackRuntimeSnapshot,
            _events: &[EngineDomainEvent],
            _effects: &[poise_engine::transition::TrackEffect],
            _effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
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
            Ok(CommittedTrackWrite {
                track_id: poise_engine::track::TrackId::new(id),
                effects: Vec::new(),
            })
        }

        async fn load_track_state(
            &self,
            id: &str,
        ) -> Result<Option<poise_engine::snapshot::TrackRuntimeSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_track_events(&self, _id: &str) -> Result<Vec<EngineDomainEvent>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl TrackEffectStore for BlockingPersistence {
        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            _track_id: &TrackId,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_track_batch(
            &self,
            _track_id: &TrackId,
            _batch_id: &str,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn save_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            _track_id: &TrackId,
        ) -> Result<Vec<FollowUpRetirementRequest>> {
            Ok(Vec::new())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TrackQueryStore for BlockingPersistence {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .values()
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: chrono::Utc::now(),
                })
                .collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .get(track_id.as_str())
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: chrono::Utc::now(),
                }))
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
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
    }

    #[async_trait::async_trait]
    impl ExecutionPort for FakeExecutionPort {
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
    }

    #[async_trait::async_trait]
    impl AccountSummaryPort for FakeAccountSummaryPort {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: chrono::Utc::now(),
            })
        }
    }

    #[async_trait::async_trait]
    impl AccountPort for FakeAccountPort {
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
    impl MetadataPort for FakeMetadataPort {
        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                rules: test_exchange_rules(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Ok(chrono::Utc::now())
        }
    }

    #[async_trait::async_trait]
    impl MarketDataPort for FakeMarketDataPort {
        async fn subscribe_prices(
            &self,
            _instrument: &Instrument,
        ) -> Result<mpsc::Receiver<PriceTick>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
