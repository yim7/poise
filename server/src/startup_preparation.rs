use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use poise_application::exchange::{ExchangeInfo, MetadataPort};
use poise_core::track::{Instrument, TrackDefinition, TrackId};
use tokio::time::{Duration, sleep};

pub(crate) type TrackLeverageIndex = HashMap<TrackId, u32>;

#[async_trait::async_trait]
pub(crate) trait ExchangeStartupControl: Send + Sync {
    async fn validate_account_mode(&self) -> Result<()> {
        Ok(())
    }

    async fn validate_instrument_mode(&self, _instrument: &Instrument) -> Result<()> {
        Ok(())
    }

    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()>;
}

pub(crate) async fn apply_exchange_startup_controls(
    track_definitions: &[TrackDefinition],
    track_leverage_index: &TrackLeverageIndex,
    exchange_startup_control: &dyn ExchangeStartupControl,
) -> Result<()> {
    if let Err(error) = exchange_startup_control.validate_account_mode().await {
        return Err(anyhow!(
            "failed to validate exchange account mode: {error:#}"
        ));
    }

    for track in track_definitions {
        let track_id = track.track_id().clone();
        let instrument = track.instrument().clone();
        let leverage = track_leverage_index
            .get(&track_id)
            .copied()
            .ok_or_else(|| anyhow!("missing startup leverage for track `{}`", track_id.as_str()))?;
        exchange_startup_control
            .validate_instrument_mode(&instrument)
            .await
            .with_context(|| {
                format!(
                    "failed to validate startup mode for track `{}` symbol `{}`",
                    track_id.as_str(),
                    instrument.symbol
                )
            })?;
        exchange_startup_control
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::{Result, anyhow};
    use poise_application::exchange::{ExchangeInfo, MetadataPort};
    use poise_core::track::{Instrument, Venue};

    use super::load_exchange_info_with_retry;

    #[tokio::test]
    async fn load_exchange_info_retries_transient_failures() {
        let metadata = FlakyMetadataPort::new(2);
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");

        let info = load_exchange_info_with_retry(&metadata, &instrument)
            .await
            .unwrap();

        assert_eq!(metadata.calls(), 3);
        assert_eq!(info.instrument_spec.instrument(), &instrument);
    }

    struct FlakyMetadataPort {
        remaining_failures: Mutex<usize>,
        calls: Mutex<usize>,
    }

    impl FlakyMetadataPort {
        fn new(remaining_failures: usize) -> Self {
            Self {
                remaining_failures: Mutex::new(remaining_failures),
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl MetadataPort for FlakyMetadataPort {
        async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
            *self.calls.lock().unwrap() += 1;
            let mut remaining_failures = self.remaining_failures.lock().unwrap();
            if *remaining_failures > 0 {
                *remaining_failures -= 1;
                return Err(anyhow!("temporary metadata failure"));
            }
            Ok(ExchangeInfo {
                instrument_spec: crate::test_support::test_instrument_spec_for(instrument.clone()),
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            Ok(chrono::Utc::now())
        }
    }
}
