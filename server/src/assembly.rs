use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use grid_binance::BinanceAdapter;
use grid_engine::grid::{GridId, Instrument};
use grid_engine::manager::GridManager;
use grid_engine::ports::{
    ClockPort, ExchangePort, GridReadRepositoryPort, MarketDataPort, StateRepositoryPort,
};
use grid_storage::sqlite::SqliteStorage;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::projector::GridProjector;
use crate::query_service::GridQueryService;
use crate::runtime::{RuntimeHandles, ServerRuntime};
use crate::write_service::GridWriteService;
#[derive(Clone)]
pub struct ServerState {
    pub write_service: Arc<GridWriteService>,
    #[allow(dead_code)]
    pub query_service: Arc<GridQueryService>,
    #[allow(dead_code)]
    pub projector: Arc<GridProjector>,
}

pub struct ServerPlatform {
    state: ServerState,
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
    validate_unique_instruments(config.grids.iter().map(|grid| grid.instrument()))?;
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
    R: StateRepositoryPort + GridReadRepositoryPort + 'static,
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
    R: StateRepositoryPort + GridReadRepositoryPort + 'static,
{
    validate_unique_instruments(config.grids.iter().map(|grid| grid.instrument()))?;
    validate_unique_grid_ids(config.grids.iter().map(|grid| grid.grid_id()))?;

    let mut manager = GridManager::new(clock);
    for grid in &config.grids {
        let grid_id = grid.grid_id();
        let info = exchange.get_exchange_info(&grid.instrument()).await?;
        manager.add_grid(
            grid_id.clone(),
            grid.instrument(),
            grid.grid_config(),
            grid.budget(),
            info.rules,
        )?;
        if let Some(snapshot) = repository.load_grid_state(grid_id.as_str()).await? {
            manager.restore_grid_state(&snapshot)?;
        }
    }

    let (notifications, _) = broadcast::channel(256);
    let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
    let read_repository: Arc<dyn GridReadRepositoryPort> = repository;
    let write_service = Arc::new(GridWriteService::new(
        manager,
        state_repository,
        notifications.clone(),
    ));
    let query_service = Arc::new(GridQueryService::new(read_repository));
    let projector = Arc::new(GridProjector::new());
    let server_state = build_server_state(write_service, query_service, projector);

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

pub(crate) fn build_server_state(
    write_service: Arc<GridWriteService>,
    query_service: Arc<GridQueryService>,
    projector: Arc<GridProjector>,
) -> ServerState {
    ServerState {
        write_service,
        query_service,
        projector,
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
    use grid_core::events::DomainEvent as EngineDomainEvent;
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::GridManager;
    use grid_engine::observation::{GridObservation, MarketObservation};
    use grid_engine::ports::{
        ExchangeInfo, ExchangeOrder, ExchangePort, GridReadRepositoryPort, GridSnapshot,
        OrderReceipt, OrderRequest, Position, PriceTick, StateRepositoryPort,
    };
    use grid_protocol::{GridStreamEvent, GridStreamPayload};
    use grid_storage::sqlite::SqliteStorage;
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};
    use tokio_tungstenite::connect_async;

    use crate::config::{Config, ExchangeConfig, GridDefinition};
    use crate::http::router;
    use crate::projector::GridProjector;
    use crate::query_service::GridQueryService;
    use crate::write_service::GridWriteService;

    use super::{
        ServerPlatform, SystemClock, assemble, build_server_state, validate_unique_grid_ids,
        validate_unique_instruments,
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
                    grid_id: "btc-core".into(),
                    venue: Venue::Binance,
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
                    grid_id: "eth-core".into(),
                    venue: Venue::Binance,
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
        let manager_handle = platform.state().write_service.manager();
        let manager = manager_handle.read().await;

        assert_eq!(manager.list_grids().len(), 2);
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
        let error =
            validate_unique_grid_ids([GridId::new("alpha"), GridId::new("alpha")]).unwrap_err();
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
                    grid_id: "btc-core".into(),
                    venue: Venue::Binance,
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
                    grid_id: "btc-alt".into(),
                    venue: Venue::Binance,
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
        assert!(error.to_string().contains("duplicate instrument"));
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
        let payload: GridStreamEvent = serde_json::from_str(message.to_text().unwrap()).unwrap();

        assert_eq!(payload.grid_id, "btc-core");
        assert!(matches!(
            payload.payload,
            GridStreamPayload::GridListItemChanged { .. }
        ));

        server.abort();
        let _ = server.await;
        handles.market_task.abort();
        handles.user_task.abort();
        handles.effect_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
        let _ = handles.effect_task.await;
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
                grid_id: "btc-core".into(),
                venue: Venue::Binance,
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
                .uri("/grids/btc-core/commands")
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
        let grid = manager.get_grid("btc-core").unwrap();

        assert_eq!(grid.status, grid_engine::runtime::GridStatus::Paused);

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
                &GridId::new("btc-core"),
                GridObservation::Market(MarketObservation {
                    reference_price: tick.reference_price,
                }),
            );
            manager.pause_grid("btc-core").unwrap();
        }

        let resume_request = tokio::spawn(async move {
            tower::ServiceExt::oneshot(
                app,
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/grids/btc-core/commands")
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
            .load_grid_state("btc-core")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.status, grid_engine::runtime::GridStatus::Frozen);
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
        R: StateRepositoryPort + GridReadRepositoryPort + 'static,
    {
        let (btc_sender, btc_receiver) = mpsc::channel(8);
        let mut receivers = HashMap::new();
        receivers.insert("BTCUSDT".to_string(), btc_receiver);
        let exchange = Arc::new(FakeExchange);
        let market_data = Arc::new(FakeMarketData {
            receivers: Mutex::new(receivers),
        });

        let mut manager = GridManager::new(Arc::new(SystemClock));
        manager
            .add_grid(
                GridId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
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
        let state_repository: Arc<dyn StateRepositoryPort> = repository.clone();
        let read_repository: Arc<dyn GridReadRepositoryPort> = repository;
        let write_service = Arc::new(GridWriteService::new(
            manager,
            state_repository,
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            Arc::new(GridQueryService::new(read_repository)),
            Arc::new(GridProjector::new()),
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

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Ok(chrono::Utc::now())
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
            _effects: &[grid_engine::transition::GridEffect],
        ) -> Result<grid_engine::ports::CommittedGridWrite> {
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
            Ok(grid_engine::ports::CommittedGridWrite {
                grid_id: grid_engine::grid::GridId::new(id),
                effects: Vec::new(),
            })
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<EngineDomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(
            &self,
        ) -> Result<Vec<grid_engine::ports::PersistedGridEffect>> {
            Ok(Vec::new())
        }

        async fn mark_effect_executing(&self, _effect_id: &str) -> Result<()> {
            Ok(())
        }

        async fn mark_effect_succeeded(&self, _effect_id: &str) -> Result<()> {
            Ok(())
        }

        async fn mark_effect_failed(&self, _effect_id: &str, _error: &str) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl GridReadRepositoryPort for BlockingPersistence {
        async fn list_grid_snapshots(&self) -> Result<Vec<grid_engine::ports::StoredGridSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .values()
                .cloned()
                .map(|snapshot| grid_engine::ports::StoredGridSnapshot {
                    snapshot,
                    updated_at: chrono::Utc::now(),
                })
                .collect())
        }

        async fn load_grid_snapshot(
            &self,
            grid_id: &GridId,
        ) -> Result<Option<grid_engine::ports::StoredGridSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .get(grid_id.as_str())
                .cloned()
                .map(|snapshot| grid_engine::ports::StoredGridSnapshot {
                    snapshot,
                    updated_at: chrono::Utc::now(),
                }))
        }

        async fn list_recent_grid_events(
            &self,
            _grid_id: &GridId,
            _limit: usize,
        ) -> Result<Vec<grid_engine::ports::StoredDomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_grid_effects(
            &self,
            _grid_id: &GridId,
            _limit: usize,
        ) -> Result<Vec<grid_engine::ports::PersistedGridEffect>> {
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
        ) -> Result<mpsc::Receiver<grid_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
