use anyhow::Result;
use grid_platform_tui::runtime::{AppConfig, load_dotenv_if_present, run_app};

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv_if_present().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    color_eyre::install().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    run_app(AppConfig::from_env()).await
}
