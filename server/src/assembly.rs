use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use grid_binance::BinanceAdapter;
use grid_engine::instance::StrategyInstance;
use grid_engine::manager::InstanceManager;
use grid_engine::ports::{
    ClockPort, ExchangePort, InstanceSnapshot, MarketDataPort, PersistencePort,
};
use grid_storage::sqlite::SqliteStorage;
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::config::Config;
use crate::runtime::{Runtime, RuntimeHandles};
use crate::websocket::WsEvent;

pub type SharedManager = Arc<RwLock<InstanceManager>>;

#[derive(Clone)]
pub struct AppState {
    pub manager: SharedManager,
    pub persistence: Arc<dyn PersistencePort>,
    pub mutation_lock: Arc<Mutex<()>>,
    pub events: broadcast::Sender<WsEvent>,
}

pub struct Platform {
    state: AppState,
    pub runtime: Runtime,
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

fn validate_unique_instance_ids<'a>(
    instance_ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), anyhow::Error> {
    let mut known_instance_ids = HashSet::new();
    for instance_id in instance_ids {
        if !known_instance_ids.insert(instance_id) {
            return Err(anyhow!("duplicate instance id `{instance_id}`"));
        }
    }
    Ok(())
}

pub async fn assemble(config: &Config) -> Result<Platform> {
    validate_unique_symbols(
        config
            .instances
            .iter()
            .map(|instance| instance.symbol.as_str()),
    )?;
    validate_unique_instance_ids(
        config
            .instances
            .iter()
            .map(|instance| instance.instance_id())
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str),
    )?;

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
    let persistence: Arc<dyn PersistencePort> = Arc::new(SqliteStorage::new(&db_path)?);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock);

    assemble_with_components(config, exchange, market_data, persistence, clock).await
}

pub(crate) async fn assemble_with_components(
    config: &Config,
    exchange: Arc<dyn ExchangePort>,
    market_data: Arc<dyn MarketDataPort>,
    persistence: Arc<dyn PersistencePort>,
    clock: Arc<dyn ClockPort>,
) -> Result<Platform> {
    validate_unique_symbols(
        config
            .instances
            .iter()
            .map(|instance| instance.symbol.as_str()),
    )?;
    validate_unique_instance_ids(
        config
            .instances
            .iter()
            .map(|instance| instance.instance_id())
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str),
    )?;

    let mut manager = InstanceManager::new(Arc::clone(&exchange), Arc::clone(&persistence), clock);
    for instance in &config.instances {
        let instance_id = instance.instance_id();
        let info = exchange.get_exchange_info(&instance.symbol).await?;
        manager.add_instance(
            instance_id.clone(),
            instance.symbol.clone(),
            instance.grid_config(),
            instance.budget(),
            info.rules,
        )?;
        if let Some(snapshot) = persistence.load_instance_state(&instance_id).await? {
            manager.restore_instance_state(&snapshot)?;
        }
    }

    let (events, _) = broadcast::channel(256);
    let app_state = build_app_state(manager, persistence, events);

    Ok(Platform {
        state: app_state.clone(),
        runtime: Runtime::new(app_state, exchange, market_data),
    })
}

impl Platform {
    pub fn app_state(&self) -> AppState {
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

struct SystemClock;

impl ClockPort for SystemClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        Utc::now()
    }
}

pub(crate) fn snapshot_from_instance(instance: &StrategyInstance) -> InstanceSnapshot {
    InstanceSnapshot {
        id: instance.id.clone(),
        symbol: instance.symbol.clone(),
        config: instance.config.clone(),
        status: instance.status.clone(),
        current_exposure: instance.current_exposure.clone(),
        target_exposure: instance.target_exposure.clone(),
        pending_order: instance.pending_order.clone(),
        risk_state: instance.risk_state.clone(),
        last_price: instance.last_price,
        out_of_band_since: instance.out_of_band_since,
    }
}

fn snapshot_for_id(manager: &InstanceManager, id: &str) -> Result<InstanceSnapshot> {
    let instance = manager
        .get_instance(id)
        .ok_or_else(|| anyhow!("instance `{id}` not found"))?;
    Ok(snapshot_from_instance(instance))
}

#[derive(Debug)]
pub(crate) enum MutateAndPersistError {
    Mutation(anyhow::Error),
    Persistence(anyhow::Error),
}

impl MutateAndPersistError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Mutation(error) | Self::Persistence(error) => error.to_string(),
        }
    }
}

pub(crate) async fn mutate_instance_and_persist<R, F>(
    state: &AppState,
    id: &str,
    mutate: F,
) -> std::result::Result<R, MutateAndPersistError>
where
    F: FnOnce(&mut InstanceManager) -> Result<R>,
{
    let _mutation_guard = state.mutation_lock.lock().await;
    let (previous_snapshot, result, next_snapshot) = {
        let mut manager = state.manager.write().await;
        let previous_snapshot =
            snapshot_for_id(&manager, id).map_err(MutateAndPersistError::Mutation)?;
        let result = mutate(&mut manager).map_err(MutateAndPersistError::Mutation)?;
        let next_snapshot =
            snapshot_for_id(&manager, id).map_err(MutateAndPersistError::Mutation)?;
        (previous_snapshot, result, next_snapshot)
    };

    if let Err(error) = state
        .persistence
        .save_instance_state(id, &next_snapshot)
        .await
    {
        let rollback_result = {
            let mut manager = state.manager.write().await;
            manager.restore_instance_state(&previous_snapshot)
        };
        if let Err(rollback_error) = rollback_result {
            return Err(MutateAndPersistError::Persistence(anyhow!(
                "failed to persist instance `{id}`: {error}; rollback failed: {rollback_error}"
            )));
        }
        return Err(MutateAndPersistError::Persistence(error));
    }

    Ok(result)
}

fn build_app_state(
    manager: InstanceManager,
    persistence: Arc<dyn PersistencePort>,
    events: broadcast::Sender<WsEvent>,
) -> AppState {
    AppState {
        manager: Arc::new(RwLock::new(manager)),
        persistence,
        mutation_lock: Arc::new(Mutex::new(())),
        events,
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
    use grid_core::events::DomainEvent;
    use grid_engine::manager::InstanceManager;
    use grid_engine::ports::{
        ExchangeInfo, ExchangePort, InstanceSnapshot, OpenOrder, OrderReceipt, OrderRequest,
        PersistencePort, Position, PriceTick, UserDataEvent,
    };
    use grid_storage::sqlite::SqliteStorage;
    use tokio::net::TcpListener;
    use tokio::sync::{Notify, broadcast, mpsc};
    use tokio_tungstenite::connect_async;

    use crate::config::{Config, ExchangeConfig, InstanceConfig};
    use crate::http::router;
    use crate::websocket::WsEvent;

    use super::{
        AppState, Platform, Runtime, SharedManager, SystemClock, assemble,
        mutate_instance_and_persist, validate_unique_instance_ids,
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
            instances: vec![
                InstanceConfig {
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_capacity: 8.0,
                    short_capacity: 8.0,
                    capacity_notional: 375.0,
                    shape_family: grid_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: grid_core::strategy::OutOfBandPolicy::Freeze,
                },
                InstanceConfig {
                    symbol: "ETHUSDT".into(),
                    lower_price: 2000.0,
                    upper_price: 2500.0,
                    long_capacity: 5.0,
                    short_capacity: 3.0,
                    capacity_notional: 500.0,
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
        let state = platform.app_state();
        let manager = state.manager.read().await;

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
    fn assemble_rejects_duplicate_instance_ids() {
        let error = validate_unique_instance_ids(["alpha", "alpha"]).unwrap_err();
        assert!(error.to_string().contains("duplicate instance id"));
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
            instances: vec![
                InstanceConfig {
                    symbol: "BTCUSDT".into(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_capacity: 8.0,
                    short_capacity: 8.0,
                    capacity_notional: 375.0,
                    shape_family: grid_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: grid_core::strategy::OutOfBandPolicy::Freeze,
                },
                InstanceConfig {
                    symbol: "BTCUSDT".into(),
                    lower_price: 80.0,
                    upper_price: 100.0,
                    long_capacity: 6.0,
                    short_capacity: 6.0,
                    capacity_notional: 250.0,
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
        let app = router(platform.app_state());

        let handles = platform.start_market_data_tasks().await.unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (client, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let (_, mut stream) = client.split();

        btc_sender
            .send(PriceTick {
                symbol: "BTCUSDT".into(),
                last_price: 95.0,
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

        assert_eq!(payload.instance_id, "BTCUSDT");
        assert!(matches!(
            payload.event,
            DomainEvent::ExposureTargetChanged { .. }
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
            instances: vec![InstanceConfig {
                symbol: "BTCUSDT".into(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
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
        let app = router(first.app_state());
        let pause = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .method("POST")
                .uri("/instances/BTCUSDT/commands")
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
        let state = second.app_state();
        let manager = state.manager.read().await;
        let instance = manager.get_instance("BTCUSDT").unwrap();

        assert_eq!(
            instance.status,
            grid_engine::instance::InstanceStatus::Paused
        );

        let _ = std::fs::remove_dir_all(std::path::Path::new(".data").join(&suffix));
    }

    #[tokio::test]
    async fn newer_tick_snapshot_is_not_overwritten_by_older_command_snapshot() {
        let persistence = Arc::new(BlockingPersistence::default());
        let (platform, _btc_sender) = test_platform_with_persistence(persistence.clone());
        let app = router(platform.app_state());

        {
            let mut manager = platform.state.manager.write().await;
            let tick = PriceTick {
                symbol: "BTCUSDT".into(),
                last_price: 95.0,
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
                    .uri("/instances/BTCUSDT/commands")
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
                last_price: 85.0,
                mark_price: 85.0,
                timestamp: chrono::Utc::now(),
            };
            mutate_instance_and_persist(&tick_state, "BTCUSDT", |manager| {
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
            .load_instance_state("BTCUSDT")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot.status,
            grid_engine::instance::InstanceStatus::Frozen
        );
        assert_eq!(snapshot.last_price, Some(85.0));
    }

    fn test_platform() -> (Platform, mpsc::Sender<PriceTick>) {
        test_platform_with_persistence(Arc::new(FakePersistence))
    }

    async fn assemble_with_fake_ports(config: &Config) -> Result<Platform> {
        let db_path = config.default_db_path();
        super::ensure_parent_dir(&db_path)?;
        let persistence: Arc<dyn PersistencePort> = Arc::new(SqliteStorage::new(&db_path)?);
        super::assemble_with_components(
            config,
            Arc::new(FakeExchange),
            Arc::new(FakeMarketData::empty()),
            persistence,
            Arc::new(SystemClock),
        )
        .await
    }

    fn test_platform_with_persistence(
        persistence: Arc<dyn PersistencePort>,
    ) -> (Platform, mpsc::Sender<PriceTick>) {
        let (btc_sender, btc_receiver) = mpsc::channel(8);
        let mut receivers = HashMap::new();
        receivers.insert("BTCUSDT".to_string(), btc_receiver);
        let exchange = Arc::new(FakeExchange);
        let market_data = Arc::new(FakeMarketData {
            receivers: Mutex::new(receivers),
        });

        let mut manager = InstanceManager::new(
            exchange.clone(),
            Arc::clone(&persistence),
            Arc::new(SystemClock),
        );
        manager
            .add_instance(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                grid_core::strategy::GridConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_capacity: 8.0,
                    short_capacity: 8.0,
                    capacity_notional: 375.0,
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
        let state = AppState {
            manager: Arc::new(tokio::sync::RwLock::new(manager)) as SharedManager,
            persistence,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
            events,
        };

        (
            Platform {
                state: state.clone(),
                runtime: Runtime::new(state, exchange, market_data),
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

        async fn get_open_orders(&self, _symbol: &str) -> Result<Vec<OpenOrder>> {
            Ok(Vec::new())
        }

        async fn get_exchange_info(&self, _symbol: &str) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                symbol: "BTCUSDT".into(),
                rules: test_exchange_rules(),
            })
        }
    }

    struct FakePersistence;

    #[async_trait::async_trait]
    impl PersistencePort for FakePersistence {
        async fn save_instance_state(
            &self,
            _id: &str,
            _state: &grid_engine::ports::InstanceSnapshot,
        ) -> Result<()> {
            Ok(())
        }

        async fn load_instance_state(
            &self,
            _id: &str,
        ) -> Result<Option<grid_engine::ports::InstanceSnapshot>> {
            Ok(None)
        }
    }

    #[derive(Default)]
    struct BlockingPersistence {
        snapshots: Mutex<HashMap<String, InstanceSnapshot>>,
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
    impl PersistencePort for BlockingPersistence {
        async fn save_instance_state(&self, id: &str, state: &InstanceSnapshot) -> Result<()> {
            let save_index = self.started_saves.fetch_add(1, Ordering::SeqCst);
            self.first_save_started.notify_waiters();
            if save_index == 0 {
                self.first_save_release.notified().await;
            }

            self.snapshots
                .lock()
                .unwrap()
                .insert(id.to_string(), state.clone());
            self.completed_saves.fetch_add(1, Ordering::SeqCst);
            self.completed_save.notify_waiters();
            Ok(())
        }

        async fn load_instance_state(&self, id: &str) -> Result<Option<InstanceSnapshot>> {
            Ok(self.snapshots.lock().unwrap().get(id).cloned())
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

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }
}
