use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use poise_core::track::Instrument;

use crate::config::{ExchangeConfig, TrackSpec};
use crate::startup_preparation::{ExchangeStartupControl, TrackLeverageIndex};

pub(crate) const DEFAULT_TRACK_LEVERAGE: u32 = 10;

#[async_trait::async_trait]
impl ExchangeStartupControl for poise_binance::SymbolLeverageControl {
    async fn validate_account_mode(&self) -> Result<()> {
        self.validate_one_way_position_mode().await
    }

    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()> {
        self.set_leverage(&instrument.symbol, leverage).await
    }
}

#[async_trait::async_trait]
impl ExchangeStartupControl for poise_bybit::SymbolLeverageControl {
    async fn validate_instrument_mode(&self, instrument: &Instrument) -> Result<()> {
        self.validate_one_way_position_mode(&instrument.symbol)
            .await
    }

    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()> {
        self.set_leverage(&instrument.symbol, leverage).await
    }
}

#[async_trait::async_trait]
impl ExchangeStartupControl for poise_hyperliquid::SymbolLeverageControl {
    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()> {
        self.set_leverage(&instrument.symbol, leverage).await
    }
}

#[async_trait::async_trait]
impl ExchangeStartupControl for poise_okx::SymbolLeverageControl {
    async fn validate_account_mode(&self) -> Result<()> {
        self.validate_net_position_mode().await
    }

    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()> {
        self.set_leverage(&instrument.symbol, leverage).await
    }
}

pub(crate) fn build_exchange_startup_control(
    config: &ExchangeConfig,
) -> Result<Arc<dyn ExchangeStartupControl>> {
    match config {
        ExchangeConfig::Binance(binance_config) => Ok(Arc::new(
            poise_binance::SymbolLeverageControl::new(binance_config)?,
        )),
        ExchangeConfig::Bybit(bybit_config) => Ok(Arc::new(
            poise_bybit::SymbolLeverageControl::new(bybit_config)?,
        )),
        ExchangeConfig::Hyperliquid(hyperliquid_config) => Ok(Arc::new(
            poise_hyperliquid::SymbolLeverageControl::new(hyperliquid_config)?,
        )),
        ExchangeConfig::Okx(okx_config) => {
            Ok(Arc::new(poise_okx::SymbolLeverageControl::new(okx_config)?))
        }
    }
}

pub(crate) fn build_track_leverage_index(tracks: &[TrackSpec]) -> Result<TrackLeverageIndex> {
    let mut index = HashMap::with_capacity(tracks.len());
    for track in tracks {
        let leverage = track.leverage.unwrap_or(DEFAULT_TRACK_LEVERAGE);
        if leverage == 0 {
            return Err(anyhow!(
                "invalid track `{}`: leverage must be positive",
                track.track_id
            ));
        }
        index.insert(track.track_id(), leverage);
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use anyhow::anyhow;
    use poise_application::TrackDefinitionRegistry;
    use poise_core::track::{Instrument, TrackId, Venue};

    use super::{build_exchange_startup_control, build_track_leverage_index};
    use crate::config::{ExchangeConfig, TrackSpec};
    use crate::startup_preparation::{ExchangeStartupControl, apply_exchange_startup_controls};

    #[test]
    fn track_leverage_index_defaults_to_ten() {
        let index = build_track_leverage_index(&[track_spec("btc-core", "BTCUSDT", None)]).unwrap();

        assert_eq!(index.get(&TrackId::new("btc-core")), Some(&10));
    }

    #[test]
    fn track_leverage_index_preserves_explicit_leverage() {
        let index =
            build_track_leverage_index(&[track_spec("btc-core", "BTCUSDT", Some(25))]).unwrap();

        assert_eq!(index.get(&TrackId::new("btc-core")), Some(&25));
    }

    #[test]
    fn track_leverage_index_stores_only_startup_fields() {
        let index =
            build_track_leverage_index(&[track_spec("btc-core", "BTCUSDT", Some(12))]).unwrap();
        let expected = HashMap::from([(TrackId::new("btc-core"), 12)]);

        assert_eq!(index, expected);
    }

    #[test]
    fn build_exchange_startup_control_accepts_binance_credentials() {
        build_exchange_startup_control(&ExchangeConfig::Binance(poise_binance::Config {
            deployment: poise_binance::Deployment::Testnet,
            api_key: Some("demo-key".into()),
            api_secret: Some("demo-secret".into()),
        }))
        .unwrap();
    }

    #[test]
    fn build_exchange_startup_control_accepts_bybit_credentials() {
        build_exchange_startup_control(&ExchangeConfig::Bybit(poise_bybit::Config {
            deployment: poise_bybit::Deployment::Testnet,
            api_key: Some("demo-key".into()),
            api_secret: Some("demo-secret".into()),
        }))
        .unwrap();
    }

    #[test]
    fn build_exchange_startup_control_accepts_hyperliquid_credentials() {
        build_exchange_startup_control(&ExchangeConfig::Hyperliquid(poise_hyperliquid::Config {
            deployment: poise_hyperliquid::Deployment::Testnet,
            private_key: Some(
                "0xe908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e".into(),
            ),
            wallet_address: Some("0x2222222222222222222222222222222222222222".into()),
            vault_address: None,
        }))
        .unwrap();
    }

    #[test]
    fn build_exchange_startup_control_accepts_okx_credentials() {
        build_exchange_startup_control(&ExchangeConfig::Okx(poise_okx::Config {
            deployment: poise_okx::Deployment::Demo,
            api_key: Some("demo-key".into()),
            api_secret: Some("demo-secret".into()),
            passphrase: Some("demo-passphrase".into()),
        }))
        .unwrap();
    }

    #[tokio::test]
    async fn apply_exchange_startup_controls_validates_then_uses_track_index_in_registry_order() {
        let tracks = vec![
            track_spec("btc-core", "BTCUSDT", Some(20)),
            track_spec("eth-core", "ETHUSDT", None),
        ];
        let registry = track_definition_registry(&tracks);
        let index = build_track_leverage_index(&tracks).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));

        apply_exchange_startup_controls(
            &registry,
            &index,
            &RecordingExchangeStartupControl::succeed(calls.clone()),
        )
        .await
        .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "validate_account_mode".to_string(),
                "validate_instrument_mode:BTCUSDT".to_string(),
                "BTCUSDT:20".to_string(),
                "validate_instrument_mode:ETHUSDT".to_string(),
                "ETHUSDT:10".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn apply_exchange_startup_controls_adds_track_symbol_and_leverage_context() {
        let tracks = vec![track_spec("btc-core", "BTCUSDT", Some(7))];
        let registry = track_definition_registry(&tracks);
        let index = build_track_leverage_index(&tracks).unwrap();

        let error = apply_exchange_startup_controls(
            &registry,
            &index,
            &RecordingExchangeStartupControl::fail("exchange rejected leverage"),
        )
        .await
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("btc-core"));
        assert!(message.contains("BTCUSDT"));
        assert!(message.contains("7"));
        assert!(message.contains("exchange rejected leverage"));
    }

    #[tokio::test]
    async fn apply_exchange_startup_controls_preserves_account_mode_error_context() {
        let tracks = vec![track_spec("btc-core", "BTCUSDT", Some(7))];
        let registry = track_definition_registry(&tracks);
        let index = build_track_leverage_index(&tracks).unwrap();

        let error = apply_exchange_startup_controls(
            &registry,
            &index,
            &RecordingExchangeStartupControl::fail_account_mode(
                "OKX account position mode must be `net_mode`, got `long_short_mode`",
            ),
        )
        .await
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("failed to validate exchange account mode"));
        assert!(message.contains("net_mode"));
        assert!(message.contains("long_short_mode"));
    }

    fn track_definition_registry(tracks: &[TrackSpec]) -> TrackDefinitionRegistry {
        let configured = tracks
            .iter()
            .map(|track| track.to_track_definition(Venue::Binance))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        TrackDefinitionRegistry::new(configured).unwrap()
    }

    fn track_spec(track_id: &str, symbol: &str, leverage: Option<u32>) -> TrackSpec {
        TrackSpec {
            track_id: track_id.into(),
            symbol: symbol.into(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: None,
            shape_family: None,
            out_of_band_policy: None,
            max_notional: None,
            leverage,
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: None,
            risk_acquisition: Default::default(),
        }
    }

    struct RecordingExchangeStartupControl {
        calls: Arc<Mutex<Vec<String>>>,
        account_mode_failure: Option<String>,
        leverage_failure: Option<String>,
    }

    impl RecordingExchangeStartupControl {
        fn succeed(calls: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                calls,
                account_mode_failure: None,
                leverage_failure: None,
            }
        }

        fn fail(message: impl Into<String>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                account_mode_failure: None,
                leverage_failure: Some(message.into()),
            }
        }

        fn fail_account_mode(message: impl Into<String>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                account_mode_failure: Some(message.into()),
                leverage_failure: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl ExchangeStartupControl for RecordingExchangeStartupControl {
        async fn validate_account_mode(&self) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push("validate_account_mode".to_string());
            if let Some(message) = &self.account_mode_failure {
                return Err(anyhow!(message.clone()));
            }
            Ok(())
        }

        async fn validate_instrument_mode(&self, instrument: &Instrument) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("validate_instrument_mode:{}", instrument.symbol));
            Ok(())
        }

        async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:{leverage}", instrument.symbol));
            if let Some(message) = &self.leverage_failure {
                return Err(anyhow!(message.clone()));
            }
            Ok(())
        }
    }
}
