use std::env;

use anyhow::Context;
use grid_platform_service::{Application, build_app};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("grid_platform_service=info,tower_http=info")),
        )
        .init();

    let addr = env::var("GRID_PLATFORM_SERVICE_ADDR").unwrap_or_else(|_| "127.0.0.1:8000".into());
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    let application = Application::bootstrap();
    info!("grid-platform service listening on {addr}");
    axum::serve(listener, build_app(application))
        .await
        .context("service stopped unexpectedly")
}
