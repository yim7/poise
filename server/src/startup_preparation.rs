use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use poise_application::TrackDefinitionRegistry;
use poise_core::track::{Instrument, TrackId};
use poise_engine::ports::{ExchangeInfo, MetadataPort};
use tokio::time::{Duration, sleep};

pub(crate) type TrackLeverageIndex = HashMap<TrackId, u32>;

#[async_trait::async_trait]
pub(crate) trait SymbolLeverageSetter: Send + Sync {
    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()>;
}

pub(crate) async fn apply_track_startup_leverage(
    prepared_registry: &TrackDefinitionRegistry,
    track_leverage_index: &TrackLeverageIndex,
    symbol_leverage_setter: &dyn SymbolLeverageSetter,
) -> Result<()> {
    for track in prepared_registry.iter() {
        let track_id = track.track_id().clone();
        let instrument = track.instrument().clone();
        let leverage = track_leverage_index
            .get(&track_id)
            .copied()
            .ok_or_else(|| anyhow!("missing startup leverage for track `{}`", track_id.as_str()))?;
        symbol_leverage_setter
            .set_leverage(&instrument, leverage)
            .await
            .map_err(|error| {
                anyhow!(
                    "failed to set startup leverage for track `{}` symbol `{}` to {}x: {}",
                    track_id.as_str(),
                    instrument.symbol,
                    leverage,
                    error
                )
            })?;
    }
    Ok(())
}

const STARTUP_RETRY_ATTEMPTS: usize = 5;
#[cfg(test)]
const STARTUP_RETRY_DELAY: Duration = Duration::from_millis(1);
#[cfg(not(test))]
const STARTUP_RETRY_DELAY: Duration = Duration::from_secs(1);

pub(crate) async fn prepare_exchange_startup_with<
    PreparedExchange,
    BuildExchange,
    BuildExchangeFuture,
    BuildSetter,
>(
    prepared_registry: &TrackDefinitionRegistry,
    track_leverage_index: &TrackLeverageIndex,
    build_exchange_fn: BuildExchange,
    build_symbol_leverage_setter_fn: BuildSetter,
) -> Result<PreparedExchange>
where
    BuildExchange: FnOnce() -> BuildExchangeFuture,
    BuildExchangeFuture: Future<Output = Result<PreparedExchange>>,
    BuildSetter: FnOnce() -> Result<Arc<dyn SymbolLeverageSetter>>,
{
    let exchange = build_exchange_fn().await?;
    let symbol_leverage_setter = build_symbol_leverage_setter_fn()?;
    apply_track_startup_leverage(
        prepared_registry,
        track_leverage_index,
        symbol_leverage_setter.as_ref(),
    )
    .await?;
    Ok(exchange)
}

pub(crate) async fn load_exchange_info_with_retry(
    metadata: &dyn MetadataPort,
    instrument: &Instrument,
) -> Result<ExchangeInfo> {
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match metadata.get_exchange_info(instrument).await {
            Ok(info) => return Ok(info),
            Err(error) => {
                if attempt + 1 == STARTUP_RETRY_ATTEMPTS {
                    return Err(error);
                }
                tracing::warn!(
                    instrument = %instrument.symbol,
                    attempt = attempt + 1,
                    max_attempts = STARTUP_RETRY_ATTEMPTS,
                    "startup exchange info probe failed: {error}"
                );
                last_error = Some(error);
            }
        }

        sleep(STARTUP_RETRY_DELAY).await;
    }

    Err(last_error.unwrap_or_else(|| anyhow!("failed to load exchange info")))
}
