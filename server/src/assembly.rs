use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use grid_binance::BinanceAdapter;
use grid_engine::key::GridId;
use grid_engine::manager::InstanceManager;
use grid_engine::ports::{ClockPort, ExchangePort, MarketDataPort, StateRepositoryPort};
use grid_storage::sqlite::SqliteStorage;
use tokio::sync::broadcast;

use crate::application::GridPlatformService;
use crate::config::Config;
use crate::runtime::{RuntimeHandles, ServerRuntime};
#[derive(Clone)]
pub struct ServerState {
    pub service: Arc<GridPlatformService>,
}

pub struct ServerPlatform {
    state: ServerState,
    pub runtime: ServerRuntime,
}

fn validate_unique_symbols<'a>(
    symbols: impl IntoIterator<Item = &'a str>,
) -> Result<(), anyhow::Error> {
    let mut known_symbols = HashSet::new();
    for symbol in symbols {
        if !known_symbols.insert(symbol) {
            return Err(anyhow!("duplicate symbol `{symbol}`"));
        }
    }
    Ok(())
}

fn validate_unique_grid_ids(
    grid_ids: impl IntoIterator<Item = GridId>,
) -> Result<(), anyhow::Error> {
    let mut known_grid_ids = HashSet::new();
    for grid_id in grid_ids {
        if !known_grid_ids.insert(grid_id.as_str().to_string()) {
            return Err(anyhow!("duplicate grid id `{}`", grid_id.as_str()));
        }
    }
    Ok(())
}

pub async fn assemble(config: &Config) -> Result<ServerPlatform> {
    validate_unique_symbols(
        config
            .grids
            .iter()
            .map(|instance| instance.symbol.as_str()),
    )?;
    validate_unique_grid_ids(config.grids.iter().map(|grid| grid.grid_id()))?;

    let adapter = Arc::new(BinanceAdapter::new(
        config.exchange.api_key.clone().unwrap_or_default(),
        config.exchange.api_secret.clone().unwrap_or_default(),
        config
            .exchange
            .rest_base_url
            .clone()
            .unwrap_or_else(|| "https://fapi.binance.com".to_string()),
        config
            .exchange
            .ws_base_url
            .clone()
            .unwrap_or_else(|| "wss://fstream.binance.com".to_string()),
    ));
    let exchange: Arc<dyn ExchangePort> = adapter.clone();
    let market_data: Arc<dyn MarketDataPort> = adapter;

    let db_path = config.default_db_path();
    ensure_parent_dir(&db_path)?;
    let repository: Arc<dyn StateRepositoryPort> = Arc::new(SqliteStorage::new(&db_path)?);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock);

    assemble_with_components(config, exchange, market_data, repository, clock).await
}

pub(crate) async fn assemble_with_components(
    config: &Config,
    exchange: Arc<dyn ExchangePort>,
    market_data: Arc<dyn MarketDataPort>,
    repository: Arc<dyn StateRepositoryPort>,
    clock: Arc<dyn ClockPort>,
) -> Result<ServerPlatform> {
    validate_unique_symbols(
        config
            .grids
            .iter()
            .map(|instance| instance.symbol.as_str()),
    )?;
    validate_unique_grid_ids(config.grids.iter().map(|grid| grid.grid_id()))?;

    let mut manager = InstanceManager::new(clock);
    for instance in &config.grids {
        let grid_id = instance.grid_id();
        let info = exchange.get_exchange_info(&instance.symbol).await?;
        manager.add_grid(
            grid_id.clone(),
            instance.symbol.clone(),
            instance.grid_config(),
            instance.budget(),
            info.rules,
        )?;
        if let Some(snapshot) = repository.load_grid_state(grid_id.as_str()).await? {
            manager.restore_instance_state(&snapshot)?;
        }
    }

    let (events, _) = broadcast::channel(256);
    let service = Arc::new(GridPlatformService::new(manager, repository, events));
    let server_state = build_server_state(service);

    Ok(ServerPlatform {
        state: server_state.clone(),
        runtime: ServerRuntime::new(server_state, exchange, market_data),
    })
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

fn build_server_state(service: Arc<GridPlatformService>) -> ServerState {
    ServerState { service }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use futures_util::StreamExt;
    use grid_core::events::DomainEvent as EngineDomainEvent;
    use grid_protocol::DomainEvent as ProtocolDomainEvent;
    use grid_engine::key::GridId;
    use grid_engine::manager::InstanceManager;
    use grid_engine::ports::{
        ExchangeInfo, ExchangePort, GridSnapshot, ExchangeOrder, OrderReceipt, OrderRequest,
        Position, PriceTick, StateRepositoryPort,
    };
    use grid_storage::sqlite::SqliteStorage;
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};
    use tokio_tungstenite::connect_async;

    use crate::application::GridPlatformService;
    use crate::config::{Config, ExchangeConfig, GridDefinition};
    use crate::http::router;
    use crate::websocket::WsEvent;

    use super::{
        ServerPlatform, ServerState, SystemClock, assemble, validate_unique_grid_ids,
    };

    fn test_exchange_rules() -> grid_core::types::ExchangeRules {
        grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
        }
    }

    #[tokio::test]
    async fn assembles_platform_with_all_instances_registered() {
        let suffix = format!(
            "test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let config = Config {
            environment: suffix.clone(),
            bind_address: "127.0.0.1:0".into(),
            grids: vec![
                GridDefinition {
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    shape_family: grid_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: grid_core::strategy::OutOfBandPolicy::Freeze,
                },
                GridDefinition {
                    symbol: "ETHUSDT".into(),
                    lower_price: 2000.0,
                    upper_price: 2500.0,
                    long_exposure_units: 5.0,
                    short_exposure_units: 3.0,
                    notional_per_unit: 500.0,
                    shape_family: grid_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: grid_core::strategy::OutOfBandPolicy::Freeze,
                },
            ],
            exchange: ExchangeConfig {
                rest_base_url: Some("http://127.0.0.1:1".into()),
                ws_base_url: Some("ws://127.0.0.1:1".into()),
                ..Default::default()
            },
        };

        let platform = assemble_with_fake_ports(&config).await.unwrap();
        let manager_handle = platform.state().service.manager();
        let manager = manager_handle.read().await;

        assert_eq!(manager.list_instances().len(), 2);
        assert!(
            std::path::Path::new(".data")
                .join(&suffix)
                .join("grid-server.sqlite")
                .exists()
        );

        let _ = std::fs::remove_dir_all(std::path::Path::new(".data").join(&suffix));
    }

    #[test]
    fn assemble_rejects_duplicate_grid_ids() {
        let error = validate_unique_grid_ids([
            GridId::from_symbol("alpha"),
            GridId::from_symbol("alpha"),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("duplicate grid id"));
    }

    #[tokio::test]
    async fn assemble_rejects_duplicate_symbols() {
        let suffix = format!(
            "test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        let config = Config {
            environment: suffix,
            bind_address: "127.0.0.1:0".into(),
            grids: vec![
                GridDefinition {
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    shape_family: grid_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: grid_core::strategy::OutOfBandPolicy::Freeze,
                },
                GridDefinition {
                    symbol: "BTCUSDT".into(),
                    lower_price: 80.0,
                    upper_price: 100.0,
                    long_exposure_units: 6.0,
                    short_exposure_units: 6.0,
                    notional_per_unit: 250.0,
                    shape_family: grid_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: grid_core::strategy::OutOfBandPolicy::Freeze,
                },
            ],
            exchange: ExchangeConfig {
                rest_base_url: Some("http://127.0.0.1:1".into()),
                ws_base_url: Some("ws://127.0.0.1:1".into()),
                ..Default::default()
            },
        };

        let error = assemble(&config).await.err().unwrap();
        assert!(error.to_string().contains("duplicate symbol"));
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
                symbol: "BTCUSDT".into(),
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
        let payload: WsEvent = serde_json::from_str(message.to_text().unwrap()).unwrap();

        assert_eq!(payload.grid_id, "BTCUSDT");
        assert!(matches!(
            payload.event,
            ProtocolDomainEvent::ExposureTargetChanged { .. }
        ));

        server.abort();
        let _ = server.await;
        handles.market_task.abort();
        handles.user_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
    }

    #[tokio::test]
    async fn pause_command_persists_across_reassembly() {
        let suffix = format!(
            "test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let config = Config {
            environment: suffix.clone(),
            bind_address: "127.0.0.1:0".into(),
            grids: vec![GridDefinition {
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: grid_core::strategy::ShapeFamily::Linear,
                out_of_band_policy: grid_core::strategy::OutOfBandPolicy::Freeze,
            }],
            exchange: ExchangeConfig {
                rest_base_url: Some("http://127.0.0.1:1".into()),
                ws_base_url: Some("ws://127.0.0.1:1".into()),
                ..Default::default()
            },
        };

        let first = assemble_with_fake_ports(&config).await.unwrap();
        let app = router(first.state());
        let pause = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/grids/BTCUSDT/commands")
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
        let manager_handle = second.state().service.manager();
        let manager = manager_handle.read().await;
        let instance = manager.get_instance("BTCUSDT").unwrap();

        assert_eq!(
            instance.status,
            grid_engine::instance::GridStatus::Paused
        );

        let _ = std::fs::remove_dir_all(std::path::Path::new(".data").join(&suffix));
    }

    #[tokio::test]
    async fn newer_tick_snapshot_is_not_overwritten_by_older_command_snapshot() {
        let persistence = Arc::new(BlockingPersistence::default());
        let (platform, _btc_sender) = test_platform_with_persistence(persistence.clone());
        let app = router(platform.state());

        {
            let manager_handle = platform.state.service.manager();
            let mut manager = manager_handle.write().await;
            let tick = PriceTick {
                symbol: "BTCUSDT".into(),
                reference_price: 95.0,
                mark_price: 95.0,
                timestamp: chrono::Utc::now(),
            };
            let _ = manager.on_price_tick(&tick);
            manager.pause_instance("BTCUSDT").unwrap();
        }

        let resume_request = tokio::spawn(async move {
            tower::ServiceExt::oneshot(
                app,
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/grids/BTCUSDT/commands")
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
                symbol: "BTCUSDT".into(),
                reference_price: 85.0,
                mark_price: 85.0,
                timestamp: chrono::Utc::now(),
            };
            tick_state.service.mutate_grid("BTCUSDT", |manager| {
                manager.on_price_tick(&tick).map(|_| ())
            })
            .await
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
            .load_grid_state("BTCUSDT")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.status,
            grid_engine::instance::GridStatus::Frozen
        );
        assert_eq!(snapshot.reference_price, Some(85.0));
    }

    fn test_platform() -> (ServerPlatform, mpsc::Sender<PriceTick>) {
        test_platform_with_persistence(Arc::new(FakePersistence))
    }

    async fn assemble_with_fake_ports(config: &Config) -> Result<ServerPlatform> {
        let db_path = config.default_db_path();
        super::ensure_parent_dir(&db_path)?;
        let repository: Arc<dyn StateRepositoryPort> = Arc::new(SqliteStorage::new(&db_path)?);
        super::assemble_with_components(
            config,
            Arc::new(FakeExchange),
            Arc::new(FakeMarketData::empty()),
            repository,
            Arc::new(SystemClock),
        )
        .await
    }

    fn test_platform_with_persistence(
        repository: Arc<dyn StateRepositoryPort>,
    ) -> (ServerPlatform, mpsc::Sender<PriceTick>) {
        let (btc_sender, btc_receiver) = mpsc::channel(8);
        let mut receivers = HashMap::new();
        receivers.insert("BTCUSDT".to_string(), btc_receiver);
        let exchange = Arc::new(FakeExchange);
        let market_data = Arc::new(FakeMarketData {
            receivers: Mutex::new(receivers),
        });

        let mut manager = InstanceManager::new(Arc::new(SystemClock));
        manager
            .add_grid(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                grid_core::strategy::GridConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    shape_family: grid_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: grid_core::strategy::OutOfBandPolicy::Freeze,
                },
                grid_core::risk::CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: -100.0,
                    stop_loss_pct: 10.0,
                },
                test_exchange_rules(),
            )
            .unwrap();
        let (events, _) = broadcast::channel(16);
        let state = ServerState {
            service: Arc::new(GridPlatformService::new(manager, repository, events)),
        };

        (
            ServerPlatform {
                state: state.clone(),
                runtime: crate::runtime::ServerRuntime::new(state, exchange, market_data),
            },
            btc_sender,
        )
    }

    struct FakeExchange;

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Err(anyhow!("not used in tests"))
        }

        async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        async fn cancel_all(&self, _symbol: &str) -> Result<()> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_position(&self, _symbol: &str) -> Result<Position> {
            Ok(Position {
                symbol: "BTCUSDT".into(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(&self, _symbol: &str) -> Result<Vec<ExchangeOrder>> {
            Ok(Vec::new())
        }

        async fn get_exchange_info(&self, _symbol: &str) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                symbol: "BTCUSDT".into(),
                rules: test_exchange_rules(),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Ok(chrono::Utc::now())
        }
    }

    struct FakePersistence;

    #[async_trait::async_trait]
    impl StateRepositoryPort for FakePersistence {
        async fn save_transition(
            &self,
            _id: &str,
            _state: &grid_engine::ports::GridSnapshot,
            _events: &[EngineDomainEvent],
        ) -> Result<()> {
            Ok(())
        }

        async fn load_grid_state(
            &self,
            _id: &str,
        ) -> Result<Option<grid_engine::ports::GridSnapshot>> {
            Ok(None)
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<EngineDomainEvent>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct BlockingPersistence {
        snapshots: AsyncMutex<HashMap<String, GridSnapshot>>,
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
        async fn save_transition(
            &self,
            id: &str,
            state: &GridSnapshot,
            _events: &[EngineDomainEvent],
        ) -> Result<()> {
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
            Ok(())
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<EngineDomainEvent>> {
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
    impl grid_engine::ports::MarketDataPort for FakeMarketData {
        async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>> {
            self.receivers
                .lock()
                .unwrap()
                .remove(symbol)
                .ok_or_else(|| anyhow!("no test receiver for symbol `{symbol}`"))
        }

        async fn subscribe_user_data(
            &self,
        ) -> Result<mpsc::Receiver<grid_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
