use std::collections::HashSet;
#[cfg(test)]
use std::path::Path;
use std::sync::Arc;

#[cfg(test)]
use anyhow::Context;
use anyhow::{Result, anyhow};
use chrono::Utc;
use poise_application::submit_effect_service::SubmitEffectService;
use poise_application::{
    AccountMonitor, ApplicationNotification, TrackCommandService, TrackDebugQueryService,
    TrackDefinitionRegistry, TrackEffectService, TrackObservationService, TrackQueryService,
    TrackRuntimeLifecycleService, TrackServiceSet,
};
use poise_binance::connect as connect_binance;
use poise_bybit::connect as connect_bybit;
use poise_core::track::{Instrument, TrackId, Venue};
use poise_engine::manager::TrackManager;
use poise_engine::ports::{ClockPort, ExchangePorts};
use poise_hyperliquid::connect as connect_hyperliquid;
use poise_okx::connect as connect_okx;
#[cfg(test)]
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tokio::time::Duration;

use crate::account_projector::AccountProjector;
use crate::config::{Config, ExchangeConfig};
use crate::exchange_freshness::ExchangeFreshness;
use crate::exchange_startup::{build_symbol_leverage_setter, build_track_leverage_index};
use crate::projector::TrackProjector;
use crate::runtime::{
    AccountMarginGuardStore, RecoveryAnomalyDirtyObserver, RecoveryDirtyState, RuntimeHandles,
    RuntimePorts, RuntimeStartupDefinition, ServerRuntime, TrackReconcileGuards,
};
use crate::server_context::{
    EffectWorkerState, HttpState, ReconcileState, RuntimeState, WebSocketState,
};
use crate::startup_preparation::{
    TrackLeverageIndex, load_exchange_info_with_retry, prepare_exchange_startup_with,
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

fn validate_registry_venue(
    track_definition_registry: &TrackDefinitionRegistry,
    exchange_venue: Venue,
) -> Result<(), anyhow::Error> {
    for definition in track_definition_registry.iter() {
        let instrument = definition.instrument();
        if instrument.venue != exchange_venue {
            return Err(anyhow!(
                "track `{}` instrument venue `{}` does not match exchange venue `{}`",
                definition.track_id().as_str(),
                instrument.venue.as_str(),
                exchange_venue.as_str()
            ));
        }
    }
    Ok(())
}

pub(crate) async fn build_exchange(config: &ExchangeConfig) -> Result<(Venue, ExchangePorts)> {
    match config {
        ExchangeConfig::Binance(binance_config) => {
            let ports = connect_binance(binance_config).await?;
            Ok((Venue::Binance, ports))
        }
        ExchangeConfig::Bybit(bybit_config) => {
            let ports = connect_bybit(bybit_config).await?;
            Ok((Venue::Bybit, ports))
        }
        ExchangeConfig::Hyperliquid(hyperliquid_config) => {
            let ports = connect_hyperliquid(hyperliquid_config).await?;
            Ok((Venue::Hyperliquid, ports))
        }
        ExchangeConfig::Okx(okx_config) => {
            let ports = connect_okx(okx_config).await?;
            Ok((Venue::Okx, ports))
        }
    }
}

pub async fn assemble(
    config: &Config,
    track_definition_registry: Arc<TrackDefinitionRegistry>,
    repositories: StateRepositories,
) -> Result<ServerPlatform> {
    let track_leverage_index = build_track_leverage_index(&config.tracks)?;
    validate_registry_venue(track_definition_registry.as_ref(), config.exchange.venue())?;
    validate_unique_instruments(
        track_definition_registry
            .iter()
            .map(|track| track.instrument().clone()),
    )?;
    validate_unique_track_ids(
        track_definition_registry
            .iter()
            .map(|track| track.track_id().clone()),
    )?;
    let (exchange_venue, exchange_ports) = build_exchange_and_prepare_startup(
        config,
        track_definition_registry.as_ref(),
        &track_leverage_index,
    )
    .await?;
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock);

    assemble_with_state_store(
        config,
        track_definition_registry,
        exchange_venue,
        exchange_ports,
        repositories,
        clock,
        &track_leverage_index,
    )
    .await
}

async fn build_exchange_and_prepare_startup(
    config: &Config,
    track_definition_registry: &TrackDefinitionRegistry,
    track_leverage_index: &TrackLeverageIndex,
) -> Result<(Venue, ExchangePorts)> {
    prepare_exchange_startup_with(
        track_definition_registry,
        track_leverage_index,
        || build_exchange(&config.exchange),
        || build_symbol_leverage_setter(&config.exchange),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn assemble_with_exchange_ports(
    config: &Config,
    track_definition_registry: Arc<TrackDefinitionRegistry>,
    exchange_ports: ExchangePorts,
    repositories: StateRepositories,
    clock: Arc<dyn ClockPort>,
) -> Result<ServerPlatform> {
    let track_leverage_index = build_track_leverage_index(&config.tracks)?;
    assemble_with_state_store(
        config,
        track_definition_registry,
        config.exchange.venue(),
        exchange_ports,
        repositories,
        clock,
        &track_leverage_index,
    )
    .await
}

async fn assemble_with_state_store(
    config: &Config,
    track_definition_registry: Arc<TrackDefinitionRegistry>,
    exchange_venue: Venue,
    exchange_ports: ExchangePorts,
    repositories: StateRepositories,
    clock: Arc<dyn ClockPort>,
    track_leverage_index: &TrackLeverageIndex,
) -> Result<ServerPlatform> {
    validate_registry_venue(track_definition_registry.as_ref(), exchange_venue)?;
    validate_unique_instruments(
        track_definition_registry
            .iter()
            .map(|track| track.instrument().clone()),
    )?;
    validate_unique_track_ids(
        track_definition_registry
            .iter()
            .map(|track| track.track_id().clone()),
    )?;

    let mut manager = TrackManager::new(clock.clone());
    let mut startup_definitions = Vec::new();
    let metadata = exchange_ports.metadata();
    for track in track_definition_registry.iter() {
        let track_id = track.track_id().clone();
        let instrument = track.instrument().clone();
        let info = load_exchange_info_with_retry(metadata.as_ref(), &instrument).await?;
        let startup_leverage = track_leverage_index
            .get(&track_id)
            .copied()
            .ok_or_else(|| anyhow!("missing startup leverage for track `{}`", track_id.as_str()))?;
        startup_definitions.push(RuntimeStartupDefinition::new(
            track.clone(),
            startup_leverage,
        ));
        manager.add_track(track.clone(), info.rules)?;
    }

    let (notifications, _) = broadcast::channel(256);
    let (live_view_notifications, _) = broadcast::channel(1024);
    let mutation_store = repositories.mutation_store();
    let query_store = repositories.query_store();
    let effect_store = repositories.effect_store();
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let recovery_dirty_state = Arc::new(RecoveryDirtyState::default());
    let write_services = TrackServiceSet::new_with_recovery_anomaly_observer(
        manager,
        mutation_store.clone(),
        query_store.clone(),
        effect_store.clone(),
        notifications.clone(),
        account_margin_guard.clone(),
        Arc::new(RecoveryAnomalyDirtyObserver::new(
            recovery_dirty_state.clone(),
        )),
    );
    let session_effect_queue = write_services.session_effect_queue.clone();
    let command_service = Arc::new(write_services.command);
    let observation_service = Arc::new(write_services.observation);
    let effect_service = Arc::new(write_services.effect);
    let submit_effect_service = Arc::new(write_services.submit_effect);
    let runtime_lifecycle_service = Arc::new(write_services.runtime_lifecycle);
    #[cfg(test)]
    let manager = observation_service.manager();
    let query_service = Arc::new(TrackQueryService::new(
        query_store.clone(),
        track_definition_registry.clone(),
        observation_service.clone(),
    ));
    let debug_query_service = Arc::new(TrackDebugQueryService::new(
        query_store,
        observation_service.clone(),
    ));
    let projector = Arc::new(TrackProjector::new());
    let account_projector = Arc::new(AccountProjector::new());
    let account_monitor = if let Some(account_store) = repositories.account_monitor_store() {
        Arc::new(
            AccountMonitor::restore(
                exchange_ports.account_summary(),
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
        runtime_lifecycle_service.clone(),
        session_effect_queue.clone(),
        exchange_freshness.clone(),
        reconcile_guards.clone(),
        submit_preflight.clone(),
        recovery_dirty_state.clone(),
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
        live_view_notifications.clone(),
        observation_service.clone(),
        query_service.clone(),
        projector.clone(),
        account_monitor.clone(),
        account_projector.clone(),
    );
    let runtime_state = build_runtime_state(
        reconcile_state.clone(),
        notifications.clone(),
        live_view_notifications.clone(),
        account_monitor.clone(),
        account_margin_guard.clone(),
    );
    let effect_worker_state = build_effect_worker_state(
        reconcile_state.clone(),
        effect_service.clone(),
        submit_effect_service.clone(),
        account_margin_guard.clone(),
        session_effect_queue,
    );
    #[cfg(test)]
    let (runtime_test_context, effect_worker_test_context) =
        build_test_contexts_from_runtime_states(
            runtime_state.clone(),
            effect_worker_state.clone(),
            notifications.clone(),
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
        runtime: ServerRuntime::with_startup_definitions(
            runtime_state,
            effect_worker_state,
            RuntimePorts::new(
                exchange_ports.execution(),
                exchange_ports.market_data(),
                exchange_ports.account_summary(),
                exchange_ports.account(),
                exchange_ports.metadata(),
                clock,
            ),
            startup_definitions,
            Duration::from_secs(1),
        ),
    })
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
    live_view_notifications: broadcast::Sender<String>,
    observation_service: Arc<TrackObservationService>,
    query_service: Arc<TrackQueryService>,
    projector: Arc<TrackProjector>,
    account_monitor: Arc<AccountMonitor>,
    account_projector: Arc<AccountProjector>,
) -> WebSocketState {
    WebSocketState {
        notifications,
        live_view_notifications,
        observation_service,
        query_service,
        projector,
        account_monitor,
        account_projector,
        #[cfg(test)]
        diagnostics_tx: None,
    }
}

pub(crate) fn build_reconcile_state(
    observation_service: Arc<TrackObservationService>,
    runtime_lifecycle_service: Arc<TrackRuntimeLifecycleService>,
    session_effect_queue: poise_application::SessionEffectQueue,
    exchange_freshness: Arc<ExchangeFreshness>,
    reconcile_guards: Arc<TrackReconcileGuards>,
    submit_preflight: Arc<SubmitPreflight>,
    recovery_dirty_state: Arc<RecoveryDirtyState>,
) -> ReconcileState {
    ReconcileState {
        observation_service,
        runtime_lifecycle_service,
        session_effect_queue,
        exchange_freshness,
        reconcile_guards,
        submit_preflight,
        recovery_dirty_state,
    }
}

pub(crate) fn build_runtime_state(
    reconcile: ReconcileState,
    notifications: broadcast::Sender<ApplicationNotification>,
    live_view_notifications: broadcast::Sender<String>,
    account_monitor: Arc<AccountMonitor>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
) -> RuntimeState {
    RuntimeState {
        reconcile,
        notifications,
        live_view_notifications,
        account_monitor,
        account_margin_guard,
    }
}

pub(crate) fn build_effect_worker_state(
    reconcile: ReconcileState,
    effect_service: Arc<TrackEffectService>,
    submit_effect_service: Arc<SubmitEffectService>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
    session_effect_queue: poise_application::SessionEffectQueue,
) -> EffectWorkerState {
    EffectWorkerState {
        reconcile,
        effect_service,
        submit_effect_service,
        account_margin_guard,
        session_effect_queue,
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
        CommittedTrackWrite, EffectJournalEntry, EffectStatusUpdate, PersistedControlMode,
        PersistedTrackEffect, StoredTrackEvent, TrackControlState, TrackEffectJournal,
        TrackMutationStore, TrackQueryStore,
    };
    use poise_core::events::DomainEvent as EngineDomainEvent;
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_engine::manager::TrackManager;
    use poise_engine::observation::{MarketObservation, TrackObservation};
    use poise_engine::ports::{
        AccountPort, AccountSummaryPort, ExchangeInfo, ExchangeOpenOrderSnapshot, ExecutionPort,
        ExecutionQuoteTick, MarketDataPort, MarketDataTick, MetadataPort, OrderReceipt,
        OrderRequest, Position,
    };
    use poise_protocol::StreamEvent;
    use poise_storage::sqlite::SqliteStorage;
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};

    use crate::runtime::AccountMarginGuardStore;
    use tokio_tungstenite::connect_async;

    use crate::config::{Config, ExchangeConfig, TrackSpec, parse_config};
    use crate::http::router;
    use crate::projector::TrackProjector;
    use crate::state_bootstrap::StateRepositories;
    use crate::test_support::{
        build_http_state as build_test_http_state, build_runtime_and_effect_worker_test_contexts,
        build_test_application_services, build_websocket_state as build_test_websocket_state,
        unavailable_account_monitor,
    };
    use poise_application::{TrackDebugQueryService, TrackDefinitionRegistry, TrackQueryService};

    use super::{
        ServerPlatform, SystemClock, assemble, build_exchange, validate_registry_venue,
        validate_unique_instruments, validate_unique_track_ids,
    };

    fn test_exchange_rules() -> poise_core::types::ExchangeRules {
        poise_core::types::ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    fn test_track_definition_registry(config: &Config) -> Arc<TrackDefinitionRegistry> {
        let configured = config
            .tracks
            .iter()
            .map(|track| track.to_track_definition(config.exchange.venue()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        Arc::new(TrackDefinitionRegistry::new(configured).unwrap())
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

        let instrument = test_track_definition_registry(&config)
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
            test_track_definition_registry(&config),
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

        let (venue, ports) = build_exchange(&config.exchange).await.unwrap();

        assert_eq!(venue, Venue::Binance);
        let _metadata = ports.metadata();
    }

    #[tokio::test]
    async fn build_exchange_uses_exchange_deployment_for_bybit_endpoint_selection() {
        let config = parse_config(
            r#"
[exchange]
venue = "bybit"
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

        let (venue, ports) = build_exchange(&config.exchange).await.unwrap();

        assert_eq!(venue, Venue::Bybit);
        let _metadata = ports.metadata();
    }

    #[tokio::test]
    async fn build_exchange_uses_exchange_deployment_for_hyperliquid_endpoint_selection() {
        let config = parse_config(
            r#"
[exchange]
venue = "hyperliquid"
deployment = "mainnet"
private_key = "0xe908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e"
wallet_address = "0x2222222222222222222222222222222222222222"

[[tracks]]
track_id = "btc-core"
symbol = "BTC"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        let (venue, ports) = build_exchange(&config.exchange).await.unwrap();

        assert_eq!(venue, Venue::Hyperliquid);
        let _metadata = ports.metadata();
    }

    #[tokio::test]
    async fn build_exchange_uses_exchange_deployment_for_okx_endpoint_selection() {
        let config = parse_config(
            r#"
[exchange]
venue = "okx"
deployment = "demo"
api_key = "demo-key"
api_secret = "demo-secret"
passphrase = "demo-passphrase"

[[tracks]]
track_id = "btc-core"
symbol = "BTC-USDT-SWAP"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        let (venue, ports) = build_exchange(&config.exchange).await.unwrap();

        assert_eq!(venue, Venue::Okx);
        let _metadata = ports.metadata();
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

        let error = assemble(
            &config,
            test_track_definition_registry(&config),
            repositories,
        )
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
                TrackSpec {
                    track_id: "btc-core".into(),
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                    out_of_band_policy: Some(poise_core::strategy::BandProtectionPolicy::Freeze),
                    max_notional: None,
                    leverage: None,
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                    tick_timeout_secs: None,
                },
                TrackSpec {
                    track_id: "eth-core".into(),
                    symbol: "ETHUSDT".into(),
                    lower_price: 2000.0,
                    upper_price: 2500.0,
                    long_exposure_units: 5.0,
                    short_exposure_units: 3.0,
                    notional_per_unit: 500.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                    out_of_band_policy: Some(poise_core::strategy::BandProtectionPolicy::Freeze),
                    max_notional: None,
                    leverage: None,
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
        assert_eq!(track.max_notional(), 3000.0);
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

    #[test]
    fn assemble_rejects_registry_venue_that_does_not_match_exchange() {
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
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();
        let mismatched_definition = config.tracks[0].to_track_definition(Venue::Bybit).unwrap();
        let registry = TrackDefinitionRegistry::new(vec![mismatched_definition]).unwrap();

        let error = validate_registry_venue(&registry, config.exchange.venue()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not match exchange venue `binance`")
        );
    }

    #[tokio::test]
    async fn assemble_rejects_duplicate_symbols() {
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![
                TrackSpec {
                    track_id: "btc-core".into(),
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                    out_of_band_policy: Some(poise_core::strategy::BandProtectionPolicy::Freeze),
                    max_notional: None,
                    leverage: None,
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                    tick_timeout_secs: None,
                },
                TrackSpec {
                    track_id: "btc-alt".into(),
                    symbol: "BTCUSDT".into(),
                    lower_price: 80.0,
                    upper_price: 100.0,
                    long_exposure_units: 6.0,
                    short_exposure_units: 6.0,
                    notional_per_unit: 250.0,
                    min_rebalance_units: Some(0.5),
                    shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                    out_of_band_policy: Some(poise_core::strategy::BandProtectionPolicy::Freeze),
                    max_notional: None,
                    leverage: None,
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
        let error = assemble(
            &config,
            test_track_definition_registry(&config),
            repositories,
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("duplicate instrument"));
    }

    #[tokio::test]
    async fn assemble_requires_exchange_credentials_for_real_runtime() {
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackSpec {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                out_of_band_policy: Some(poise_core::strategy::BandProtectionPolicy::Freeze),
                max_notional: None,
                leverage: None,
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };

        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        let repositories = StateRepositories::new(repository);
        let error = assemble(
            &config,
            test_track_definition_registry(&config),
            repositories,
        )
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
            .send(MarketDataTick::ExecutionQuote(ExecutionQuoteTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                execution_quote: poise_engine::ports::ExecutionQuote {
                    best_bid: 95.0,
                    best_ask: 95.0,
                },
                timestamp: chrono::Utc::now(),
            }))
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
        handles.submit_preflight_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
        let _ = handles.effect_task.await;
        let _ = handles.recovery_task.await;
        let _ = handles.submit_preflight_task.await;
    }

    #[tokio::test]
    async fn pause_command_persists_across_reassembly() {
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackSpec {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                out_of_band_policy: Some(poise_core::strategy::BandProtectionPolicy::Freeze),
                max_notional: None,
                leverage: None,
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
        let handles = second.start_market_data_tasks().await.unwrap();
        let manager_handle = second.manager();
        let status = {
            let manager = manager_handle.read().await;
            manager.get_track("btc-core").unwrap().status()
        };

        assert_eq!(status, poise_engine::runtime::TrackStatus::Paused);

        handles.market_task.abort();
        handles.market_data_health_task.abort();
        handles.user_task.abort();
        handles.effect_task.abort();
        handles.recovery_task.abort();
        handles.submit_preflight_task.abort();
        handles.account_task.abort();
    }

    #[tokio::test]
    async fn reassembly_uses_persistent_control_state_not_runtime_snapshot_status() {
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackSpec {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                out_of_band_policy: Some(poise_core::strategy::BandProtectionPolicy::Freeze),
                max_notional: None,
                leverage: None,
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };

        let instance_dir = tempfile::tempdir().unwrap();
        let db_path = crate::instance_dir::InstanceDir::new(instance_dir.path()).db_path();
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let repository = Arc::new(SqliteStorage::new(&db_path).unwrap());

        let mut manager = TrackManager::new(Arc::new(SystemClock));
        manager
            .add_track(
                TrackDefinition::try_new(
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
                        out_of_band_policy: poise_core::strategy::BandProtectionPolicy::Freeze,
                    },
                    Some(3000.0),
                    poise_core::risk::LossLimits {
                        daily_loss_limit: 300.0,
                        total_loss_limit: 600.0,
                    },
                    None,
                )
                .unwrap(),
                test_exchange_rules(),
            )
            .unwrap();
        TrackMutationStore::save_track_control_state(
            repository.as_ref(),
            &TrackId::new("btc-core"),
            &TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            },
        )
        .await
        .unwrap();

        let second = assemble_with_fake_ports(&config, instance_dir.path())
            .await
            .unwrap();
        let handles = second.start_market_data_tasks().await.unwrap();
        let manager_handle = second.manager();
        let status = {
            let manager = manager_handle.read().await;
            manager.get_track("btc-core").unwrap().status()
        };

        assert_eq!(status, poise_engine::runtime::TrackStatus::Paused);

        handles.market_task.abort();
        handles.market_data_health_task.abort();
        handles.user_task.abort();
        handles.effect_task.abort();
        handles.recovery_task.abort();
        handles.submit_preflight_task.abort();
        handles.account_task.abort();
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

        assert!(state.session_effect_queue.claim_next().is_none());
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
            let tick = ExecutionQuoteTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                execution_quote: poise_engine::ports::ExecutionQuote {
                    best_bid: 95.0,
                    best_ask: 95.0,
                },
                timestamp: chrono::Utc::now(),
            };
            let _ = manager.observe(
                &TrackId::new("btc-core"),
                TrackObservation::Market(MarketObservation::ExecutionQuote {
                    execution_quote: tick.execution_quote,
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

        tokio::time::timeout(
            Duration::from_secs(1),
            persistence.wait_for_first_save_start(),
        )
        .await
        .expect("resume command should start its persistence write");

        let tick_state = platform.runtime_test_context();
        let tick_request = tokio::spawn(async move {
            let tick = ExecutionQuoteTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                execution_quote: poise_engine::ports::ExecutionQuote {
                    best_bid: 85.0,
                    best_ask: 85.0,
                },
                timestamp: chrono::Utc::now(),
            };
            tick_state
                .observe_market("btc-core", tick.execution_quote.best_bid)
                .await
                .map(|_| ())
        });

        let second_save_started = tokio::time::timeout(
            Duration::from_millis(100),
            persistence.wait_for_started_saves(2),
        )
        .await;
        persistence.release_first_save();

        let response = tokio::time::timeout(Duration::from_secs(1), resume_request)
            .await
            .expect("resume request should finish after the first save is released")
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(1), tick_request)
            .await
            .expect("newer tick should finish after the command save is released")
            .unwrap()
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            persistence.wait_for_completed_saves(1),
        )
        .await
        .expect("command persistence write should complete");
        assert!(
            second_save_started.is_err(),
            "newer tick must not begin a persistence write before the command save finishes"
        );

        let snapshot = platform
            .manager()
            .read()
            .await
            .mutation_frame("btc-core")
            .unwrap();
        assert_eq!(
            snapshot.status(),
            poise_engine::runtime::TrackStatus::Frozen
        );
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

    fn test_platform() -> (ServerPlatform, mpsc::Sender<MarketDataTick>) {
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
            test_track_definition_registry(config),
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
    ) -> (ServerPlatform, mpsc::Sender<MarketDataTick>)
    where
        R: TrackMutationStore + TrackEffectJournal + TrackQueryStore + 'static,
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
                TrackDefinition::try_new(
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
                        out_of_band_policy: poise_core::strategy::BandProtectionPolicy::Freeze,
                    },
                    Some(3000.0),
                    poise_core::risk::LossLimits {
                        daily_loss_limit: 100.0,
                        total_loss_limit: 300.0,
                    },
                    None,
                )
                .unwrap(),
                test_exchange_rules(),
            )
            .unwrap();
        let (events, _) = broadcast::channel(16);
        let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
        let effect_store: Arc<dyn TrackEffectJournal> = repository.clone();
        let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
        let services = build_test_application_services(
            manager,
            mutation_store.clone(),
            repository.clone() as Arc<dyn TrackQueryStore>,
            effect_store.clone(),
            events.clone(),
            account_margin_guard.clone(),
        );
        let query_store = repository.clone() as Arc<dyn TrackQueryStore>;
        let query_service = Arc::new(TrackQueryService::new(
            query_store.clone(),
            crate::test_support::test_track_definition_registry("btc-core"),
            services.observation_service.clone(),
        ));
        let debug_query_service = Arc::new(TrackDebugQueryService::new(
            query_store,
            services.observation_service.clone(),
        ));
        let projector = Arc::new(TrackProjector::new());
        let account_monitor = unavailable_account_monitor(events.clone());
        let account_projector = Arc::new(crate::account_projector::AccountProjector::new());
        let (runtime_test_context, effect_worker_test_context) =
            build_runtime_and_effect_worker_test_contexts(
                &services,
                repository.clone() as Arc<dyn TrackQueryStore>,
                effect_store,
                account_monitor.clone(),
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
                        exchange.clone(),
                        exchange,
                        Arc::new(SystemClock),
                    ),
                    vec![crate::runtime::RuntimeStartupDefinition::new(
                        crate::test_support::test_track_definition_registry("btc-core")
                            .get(&TrackId::new("btc-core"))
                            .unwrap()
                            .clone(),
                        10,
                    )],
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
        async fn submit_order(
            &self,
            _req: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            Err(poise_engine::ports::ExecutionPortError::failed(
                "not used in tests",
            ))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            Err(poise_engine::ports::ExecutionPortError::failed(
                "not used in tests",
            ))
        }

        async fn cancel_all(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<()> {
            Err(poise_engine::ports::ExecutionPortError::failed(
                "not used in tests",
            ))
        }

        async fn get_position(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<Position> {
            Ok(Position {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<ExchangeOpenOrderSnapshot> {
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                Vec::new(),
            ))
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

    #[derive(Default)]
    struct BlockingPersistence {
        control_states: AsyncMutex<HashMap<String, TrackControlState>>,
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
        async fn commit_track_transition(
            &self,
            id: &str,
            control_state: Option<&TrackControlState>,
            _events: &[EngineDomainEvent],
        ) -> Result<CommittedTrackWrite> {
            let save_index = self.started_saves.fetch_add(1, Ordering::SeqCst);
            self.first_save_started.notify_waiters();
            if save_index == 0 {
                self.first_save_release.notified().await;
            }

            let track_id = TrackId::new(id);
            if let Some(control_state) = control_state {
                self.save_track_control_state(&track_id, control_state)
                    .await?;
            }
            self.completed_saves.fetch_add(1, Ordering::SeqCst);
            self.completed_save.notify_waiters();
            Ok(CommittedTrackWrite { track_id })
        }

        async fn list_track_events(&self, _id: &str) -> Result<Vec<EngineDomainEvent>> {
            Ok(Vec::new())
        }

        async fn save_track_control_state(
            &self,
            track_id: &TrackId,
            state: &TrackControlState,
        ) -> Result<()> {
            self.control_states
                .lock()
                .await
                .insert(track_id.as_str().to_string(), state.clone());
            Ok(())
        }

        async fn insert_track_pnl_record(
            &self,
            _track_id: &TrackId,
            _record: &poise_engine::ledger::TrackPnlRecord,
        ) -> Result<bool> {
            Ok(true)
        }
    }

    #[async_trait::async_trait]
    impl TrackEffectJournal for BlockingPersistence {
        async fn append_entries(&self, _entries: &[EffectJournalEntry]) -> Result<()> {
            Ok(())
        }

        async fn record_effect_outcomes(&self, _outcomes: &[EffectStatusUpdate]) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TrackQueryStore for BlockingPersistence {
        async fn load_track_updated_at(
            &self,
            _track_id: &TrackId,
        ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
            Ok(None)
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

        async fn load_track_control_state(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<TrackControlState>> {
            Ok(self
                .control_states
                .lock()
                .await
                .get(track_id.as_str())
                .cloned())
        }

        async fn load_track_pnl_stats(
            &self,
            _track_id: &TrackId,
            pnl_utc_day: chrono::NaiveDate,
        ) -> Result<poise_engine::ledger::TrackPnlStats> {
            Ok(poise_engine::ledger::TrackPnlStats {
                pnl_utc_day,
                ..poise_engine::ledger::TrackPnlStats::default()
            })
        }
    }

    struct FakeMarketData {
        receivers: Mutex<HashMap<String, mpsc::Receiver<MarketDataTick>>>,
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
        ) -> Result<mpsc::Receiver<MarketDataTick>> {
            self.receivers
                .lock()
                .unwrap()
                .remove(&instrument.symbol)
                .ok_or_else(|| anyhow!("no test receiver for symbol `{}`", instrument.symbol))
        }
    }

    #[async_trait::async_trait]
    impl ExecutionPort for FakeExecutionPort {
        async fn submit_order(
            &self,
            _req: OrderRequest,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            Err(poise_engine::ports::ExecutionPortError::failed(
                "not used in tests",
            ))
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> poise_engine::ports::ExecutionResult<OrderReceipt> {
            Err(poise_engine::ports::ExecutionPortError::failed(
                "not used in tests",
            ))
        }

        async fn cancel_all(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<()> {
            Err(poise_engine::ports::ExecutionPortError::failed(
                "not used in tests",
            ))
        }

        async fn get_position(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<Position> {
            Ok(Position {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::ports::ExecutionResult<ExchangeOpenOrderSnapshot> {
            Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
                Vec::new(),
            ))
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
        ) -> Result<mpsc::Receiver<MarketDataTick>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
