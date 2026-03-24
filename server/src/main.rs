mod assembly;
mod config;
mod http;
mod runtime;
mod websocket;

use std::env;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("grid-server starting");

    let config_path = parse_config_path(env::args().skip(1))?;
    let config = config::load_config(&config_path)?;
    let platform = assembly::assemble(&config).await?;
    let _runtime_handles = platform.runtime.start().await?;

    let app = http::router(platform.app_state());
    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn parse_config_path(mut args: impl Iterator<Item = String>) -> Result<String> {
    let mut config_path = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing value for --config"))?;
                config_path = Some(value);
            }
            other => {
                return Err(anyhow::anyhow!("unknown argument: {other}"));
            }
        }
    }

    config_path.ok_or_else(|| anyhow::anyhow!("missing required --config <path>"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};
    use chrono::Utc;
    use grid_core::types::ExchangeRules;
    use grid_engine::ports::{
        ClockPort, ExchangeInfo, ExchangePort, OpenOrder, OrderReceipt, OrderRequest,
        PersistencePort, Position, PriceTick,
    };
    use tokio::sync::mpsc;

    use super::parse_config_path;

    #[test]
    fn parse_config_path_requires_config_flag() {
        let error = parse_config_path(Vec::<String>::new().into_iter()).unwrap_err();
        assert!(error.to_string().contains("--config"));
    }

    #[test]
    fn parse_config_path_reads_flag_value() {
        let path = parse_config_path(
            vec!["--config".to_string(), "configs/test.toml".to_string()].into_iter(),
        )
        .unwrap();
        assert_eq!(path, "configs/test.toml");
    }

    #[test]
    fn parse_config_path_rejects_unknown_arguments() {
        let error = parse_config_path(vec!["--bogus".to_string()].into_iter()).unwrap_err();
        assert!(error.to_string().contains("unknown argument"));
    }

    #[tokio::test]
    async fn startup_flow_serves_instances_and_snapshots() {
        let suffix = format!(
            "test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind_address = listener.local_addr().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("test.toml");
        fs::write(
            &config_path,
            format!(
                r#"
environment = "{suffix}"
bind_address = "{bind_address}"

[exchange]
rest_base_url = "http://127.0.0.1:1"
ws_base_url = "ws://127.0.0.1:1"

[[instances]]
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_capacity = 8.0
short_capacity = 8.0
capacity_notional = 375.0
"#
            ),
        )
        .unwrap();

        let config = crate::config::load_config(config_path.to_str().unwrap()).unwrap();
        let platform = crate::assembly::assemble_with_components(
            &config,
            Arc::new(FakeExchange),
            Arc::new(FakeMarketData::default()),
            Arc::new(FakePersistence),
            Arc::new(FakeClock),
        )
        .await
        .unwrap();
        let runtime_handles = platform.runtime.start().await.unwrap();
        let app = crate::http::router(platform.app_state());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let instances = client
            .get(format!("http://{bind_address}/instances"))
            .send()
            .await
            .unwrap();
        assert!(instances.status().is_success());
        let list: Vec<crate::http::InstanceSummary> = instances.json().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "BTCUSDT");

        let snapshot = client
            .get(format!("http://{bind_address}/instances/BTCUSDT/snapshot"))
            .send()
            .await
            .unwrap();
        assert!(snapshot.status().is_success());
        let payload: crate::http::InstanceSnapshot = snapshot.json().await.unwrap();
        assert_eq!(payload.id, "BTCUSDT");
        assert_eq!(payload.symbol, "BTCUSDT");

        server.abort();
        let _ = server.await;
        runtime_handles.market_task.abort();
        runtime_handles.user_task.abort();
        let _ = runtime_handles.market_task.await;
        let _ = runtime_handles.user_task.await;
        let _ = fs::remove_dir_all(std::path::Path::new(".data").join(&suffix));
    }

    struct FakeExchange;

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Ok(OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: "NEW".into(),
            })
        }

        async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self, _symbol: &str) -> Result<()> {
            Ok(())
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
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.1,
                    min_qty: 0.0,
                    min_notional: 0.0,
                },
            })
        }
    }

    #[derive(Default)]
    struct FakeMarketData {
        price_receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
    }

    #[async_trait::async_trait]
    impl grid_engine::ports::MarketDataPort for FakeMarketData {
        async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>> {
            self.price_receivers
                .lock()
                .unwrap()
                .remove(symbol)
                .ok_or_else(|| anyhow!("missing price receiver for {symbol}"))
        }

        async fn subscribe_user_data(&self) -> Result<grid_engine::ports::UserDataSubscription> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(grid_engine::ports::UserDataSubscription::from_receiver(
                receiver, 1,
            ))
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

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }
}
