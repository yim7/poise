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
    for instance in &config.instances {
        manager.add_instance(
            instance.instance_id(),
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
    use crate::config::{Config, ExchangeConfig, InstanceConfig};

    use super::assemble;

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
}
