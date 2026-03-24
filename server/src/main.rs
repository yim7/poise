mod assembly;
mod config;
mod http;
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
    platform.start_market_data_tasks().await;

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
    use std::fs;

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
        let platform = crate::assembly::assemble(&config).await.unwrap();
        platform.start_market_data_tasks().await;
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
        let _ = fs::remove_dir_all(std::path::Path::new(".data").join(&suffix));
    }
}
