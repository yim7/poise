mod application;
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

    let app = http::router(platform.state());
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
    use grid_engine::grid::{Instrument, Venue};
    use grid_engine::ports::{
        ClockPort, ExchangeInfo, ExchangeOrder, ExchangePort, OrderReceipt, OrderRequest,
        OrderStatus, Position, PriceTick, StateRepositoryPort,
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
    async fn startup_flow_serves_grids_and_snapshots() {
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

[[grids]]
grid_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
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
        let app = crate::http::router(platform.state());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let grids = client
            .get(format!("http://{bind_address}/grids"))
            .send()
            .await
            .unwrap();
        assert!(grids.status().is_success());
        let list: Vec<crate::http::GridSummary> = grids.json().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "btc-core");

        let snapshot = client
            .get(format!("http://{bind_address}/grids/btc-core/snapshot"))
            .send()
            .await
            .unwrap();
        assert!(snapshot.status().is_success());
        let payload: crate::http::GridSnapshot = snapshot.json().await.unwrap();
        assert_eq!(payload.id, "btc-core");
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
                status: OrderStatus::New,
            })
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Ok(())
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
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.1,
                    min_qty: 0.0,
                    min_notional: 0.0,
                },
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }

    #[derive(Default)]
    struct FakeMarketData {
        price_receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
    }

    #[async_trait::async_trait]
    impl grid_engine::ports::MarketDataPort for FakeMarketData {
        async fn subscribe_prices(
            &self,
            instrument: &Instrument,
        ) -> Result<mpsc::Receiver<PriceTick>> {
            self.price_receivers
                .lock()
                .unwrap()
                .remove(&instrument.symbol)
                .ok_or_else(|| anyhow!("missing price receiver for {}", instrument.symbol))
        }

        async fn subscribe_user_data(
            &self,
        ) -> Result<mpsc::Receiver<grid_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    struct FakePersistence;

    #[async_trait::async_trait]
    impl StateRepositoryPort for FakePersistence {
        async fn save_transition(
            &self,
            _id: &str,
            _state: &grid_engine::ports::GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
        ) -> Result<()> {
            Ok(())
        }

        async fn load_grid_state(
            &self,
            _id: &str,
        ) -> Result<Option<grid_engine::ports::GridSnapshot>> {
            Ok(None)
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }
    }

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }
}
