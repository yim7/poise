use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use poise_engine::track::Instrument;
use poise_engine::track::TrackId;

use crate::config::{ExchangeConfig, TrackFileDefinition};

pub(crate) const DEFAULT_TRACK_LEVERAGE: u32 = 10;
pub(crate) type TrackLeverageIndex = HashMap<TrackId, u32>;

#[async_trait::async_trait]
pub(crate) trait SymbolLeverageSetter: Send + Sync {
    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()>;
}

enum VenueSymbolLeverageSetter {
    Binance(poise_binance::SymbolLeverageControl),
    Bybit(poise_bybit::SymbolLeverageControl),
}

#[async_trait::async_trait]
impl SymbolLeverageSetter for VenueSymbolLeverageSetter {
    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()> {
        match self {
            Self::Binance(control) => control.set_leverage(&instrument.symbol, leverage).await,
            Self::Bybit(control) => control.set_leverage(&instrument.symbol, leverage).await,
        }
    }
}

pub(crate) fn build_symbol_leverage_setter(
    config: &ExchangeConfig,
) -> Result<Arc<dyn SymbolLeverageSetter>> {
    Ok(Arc::new(build_venue_symbol_leverage_setter(config)?))
}

fn build_venue_symbol_leverage_setter(config: &ExchangeConfig) -> Result<VenueSymbolLeverageSetter> {
    match config {
        ExchangeConfig::Binance(binance_config) => Ok(VenueSymbolLeverageSetter::Binance(
            poise_binance::SymbolLeverageControl::new(binance_config)?,
        )),
        ExchangeConfig::Bybit(bybit_config) => Ok(VenueSymbolLeverageSetter::Bybit(
            poise_bybit::SymbolLeverageControl::new(bybit_config)?,
        )),
    }
}

pub(crate) fn build_track_leverage_index(tracks: &[TrackFileDefinition]) -> Result<TrackLeverageIndex> {
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

    use poise_engine::track::TrackId;

    use super::{VenueSymbolLeverageSetter, build_track_leverage_index, build_venue_symbol_leverage_setter};
    use crate::config::{ExchangeConfig, TrackDefinition};

    #[test]
    fn track_leverage_index_defaults_to_ten() {
        let index = build_track_leverage_index(&[track_definition(None)]).unwrap();

        assert_eq!(index.get(&TrackId::new("btc-core")), Some(&10));
    }

    #[test]
    fn track_leverage_index_preserves_explicit_leverage() {
        let index = build_track_leverage_index(&[track_definition(Some(25))]).unwrap();

        assert_eq!(index.get(&TrackId::new("btc-core")), Some(&25));
    }

    #[test]
    fn track_leverage_index_stores_only_startup_fields() {
        let index = build_track_leverage_index(&[track_definition(Some(12))]).unwrap();
        let expected = HashMap::from([(TrackId::new("btc-core"), 12)]);

        assert_eq!(index, expected);
    }

    fn track_definition(leverage: Option<u32>) -> TrackDefinition {
        TrackDefinition {
            track_id: "btc-core".into(),
            symbol: "BTCUSDT".into(),
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
        }
    }

    #[test]
    fn build_symbol_leverage_setter_uses_binance_helper() {
        let setter = build_venue_symbol_leverage_setter(&ExchangeConfig::Binance(
            poise_binance::Config {
                deployment: poise_binance::Deployment::Testnet,
                api_key: Some("demo-key".into()),
                api_secret: Some("demo-secret".into()),
            },
        ))
        .unwrap();

        assert!(matches!(setter, VenueSymbolLeverageSetter::Binance(_)));
    }

    #[test]
    fn build_symbol_leverage_setter_uses_bybit_helper() {
        let setter = build_venue_symbol_leverage_setter(&ExchangeConfig::Bybit(
            poise_bybit::Config {
                deployment: poise_bybit::Deployment::Testnet,
                api_key: Some("demo-key".into()),
                api_secret: Some("demo-secret".into()),
            },
        ))
        .unwrap();

        assert!(matches!(setter, VenueSymbolLeverageSetter::Bybit(_)));
    }
}
