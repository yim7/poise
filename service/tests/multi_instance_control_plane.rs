use anyhow::Result;
use axum::{Router, body::Body, http::Request};
use grid_platform_service::{
    Application, ApplicationRegistry, build_app,
    protocol::{GridConfig, HttpSuccessEnvelope, RuntimeSnapshot},
    storage::PersistedRuntime,
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instances_endpoint_lists_symbols_from_registry() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let response = decode_json::<HttpSuccessEnvelope<Value>>(
        app,
        Request::builder()
            .uri("/instances")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    let symbols = response.data["instances"]
        .as_array()
        .expect("instances array")
        .iter()
        .filter_map(|instance| instance["symbol"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(symbols, vec!["BTCUSDT", "ETHUSDT"]);
    assert_eq!(response.data["default_symbol"], "BTCUSDT");
    assert_eq!(response.data["environment"], "testnet");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_scoped_snapshot_returns_target_symbol_only() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let snapshot = decode_json::<HttpSuccessEnvelope<RuntimeSnapshot>>(
        app,
        Request::builder()
            .uri("/instances/ETHUSDT/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    assert_eq!(snapshot.data.runtime.symbol, "ETHUSDT");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_runtime_snapshot_alias_uses_default_symbol() -> Result<()> {
    let app = bootstrap_multi_instance_app(["BTCUSDT", "ETHUSDT"])?;

    let snapshot = decode_json::<HttpSuccessEnvelope<RuntimeSnapshot>>(
        app,
        Request::builder()
            .uri("/runtime/snapshot")
            .body(Body::empty())
            .expect("request"),
    )
    .await?;

    assert_eq!(snapshot.data.runtime.symbol, "BTCUSDT");

    Ok(())
}

fn bootstrap_multi_instance_app<const N: usize>(symbols: [&str; N]) -> Result<Router> {
    let instances = symbols
        .into_iter()
        .map(|symbol| {
            (
                symbol.to_string(),
                Application::bootstrap_with_runtime(seed_runtime(symbol), symbol),
            )
        })
        .collect::<Vec<_>>();
    let registry = ApplicationRegistry::new("testnet", "BTCUSDT", instances)?;
    Ok(build_app(registry))
}

fn seed_runtime(symbol: &str) -> PersistedRuntime {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::empty_bootstrap();
    runtime.snapshot.runtime.symbol = symbol.into();
    runtime.snapshot.runtime.env = "testnet".into();
    runtime.snapshot.runtime.last_price = 100.0;
    runtime.snapshot.runtime.mark_price = 100.0;
    runtime.snapshot.strategy.config = GridConfig {
        lower_price: 90.0,
        upper_price: 110.0,
        grid_levels: 6,
        max_position_notional: 3000.0,
    };
    runtime.snapshot.execution.open_orders.clear();
    runtime.snapshot.execution.recent_fills.clear();
    runtime
}

async fn decode_json<T>(app: Router, request: Request<Body>) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let response = app.oneshot(request).await?;
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}
