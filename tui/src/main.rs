use anyhow::Result;
use grid_platform_tui::runtime::{AppConfig, run_app};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    run_app(AppConfig::from_env()).await
}
