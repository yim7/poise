use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use grid_binance::BinanceAdapter;
use grid_engine::manager::InstanceManager;
use grid_engine::ports::{ClockPort, ExchangePort, MarketDataPort};
use grid_storage::sqlite::SqliteStorage;
use tokio::sync::{RwLock, broadcast};

use crate::config::Config;
use crate::websocket::WsEvent;

pub type SharedManager = Arc<RwLock<InstanceManager>>;

#[derive(Clone)]
pub struct AppState {
    pub manager: SharedManager,
    pub events: broadcast::Sender<WsEvent>,
}

pub struct Platform {
    state: AppState,
    market_data: Arc<dyn MarketDataPort>,
}

pub async fn assemble(config: &Config) -> Result<Platform> {
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
    let persistence = Arc::new(SqliteStorage::new(&db_path)?);
    let clock = Arc::new(SystemClock);

    let mut manager = InstanceManager::new(exchange, persistence, clock);
    let mut instance_ids = HashSet::new();
    for instance in &config.instances {
        let instance_id = instance.instance_id();
        if !instance_ids.insert(instance_id.clone()) {
            return Err(anyhow::anyhow!("duplicate instance id `{instance_id}`"));
        }
        manager.add_instance(
            instance_id,
            instance.symbol.clone(),
            instance.grid_config(),
            instance.budget(),
        )?;
    }

    let (events, _) = broadcast::channel(256);

    Ok(Platform {
        state: AppState {
            manager: Arc::new(RwLock::new(manager)),
            events,
        },
        market_data,
    })
}

impl Platform {
    pub fn app_state(&self) -> AppState {
        self.state.clone()
    }

    pub async fn start_market_data_tasks(&self) {
        let symbols = {
            let manager = self.state.manager.read().await;
            manager
                .list_instances()
                .into_iter()
                .map(|instance| instance.symbol.clone())
                .collect::<HashSet<_>>()
        };

        for symbol in symbols {
            let manager = Arc::clone(&self.state.manager);
            let events = self.state.events.clone();
            let market_data = Arc::clone(&self.market_data);

            tokio::spawn(async move {
                match market_data.subscribe_prices(&symbol).await {
                    Ok(mut receiver) => {
                        while let Some(tick) = receiver.recv().await {
                            let emitted_events = {
                                let mut manager = manager.write().await;
                                manager.on_price_tick(&tick)
                            };

                            for event in emitted_events {
                                let _ = events.send(WsEvent {
                                    instance_id: symbol.clone(),
                                    event,
                                });
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!("failed to subscribe market data for {symbol}: {error}");
                    }
                }
            });
        }
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use futures_util::StreamExt;
    use grid_core::events::DomainEvent;
    use grid_engine::manager::InstanceManager;
    use grid_engine::ports::{
        ExchangeInfo, ExchangePort, OpenOrder, OrderReceipt, OrderRequest, PersistencePort,
        Position, PriceTick, UserDataEvent,
    };
    use tokio::net::TcpListener;
    use tokio::sync::{broadcast, mpsc};
    use tokio_tungstenite::connect_async;

    use crate::config::{Config, ExchangeConfig, InstanceConfig};
    use crate::http::router;
    use crate::websocket::WsEvent;

    use super::{AppState, Platform, SharedManager, SystemClock, assemble};

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

        let platform = assemble(&config).await.unwrap();
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

    #[tokio::test]
    async fn assemble_rejects_duplicate_instance_ids() {
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
        assert!(error.to_string().contains("duplicate instance id"));
    }

    #[tokio::test]
    async fn start_market_data_tasks_broadcasts_events_to_ws_clients() {
        let (platform, btc_sender) = test_platform();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(platform.app_state());

        platform.start_market_data_tasks().await;
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
    }

    fn test_platform() -> (Platform, mpsc::Sender<PriceTick>) {
        let (btc_sender, btc_receiver) = mpsc::channel(8);
        let mut receivers = HashMap::new();
        receivers.insert("BTCUSDT".to_string(), btc_receiver);

        let mut manager = InstanceManager::new(
            Arc::new(FakeExchange),
            Arc::new(FakePersistence),
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
                    max_notional: 375.0,
                    daily_loss_limit: -100.0,
                    stop_loss_pct: 10.0,
                },
            )
            .unwrap();
        let (events, _) = broadcast::channel(16);

        (
            Platform {
                state: AppState {
                    manager: Arc::new(tokio::sync::RwLock::new(manager)) as SharedManager,
                    events,
                },
                market_data: Arc::new(FakeMarketData {
                    receivers: Mutex::new(receivers),
                }),
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

        async fn cancel_all(&self, _symbol: &str) -> Result<Vec<String>> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_position(&self, _symbol: &str) -> Result<Position> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_open_orders(&self, _symbol: &str) -> Result<Vec<OpenOrder>> {
            Err(anyhow!("not used in tests"))
        }

        async fn get_exchange_info(&self, _symbol: &str) -> Result<ExchangeInfo> {
            Err(anyhow!("not used in tests"))
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

    struct FakeMarketData {
        receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
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
