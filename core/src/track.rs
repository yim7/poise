use serde::{Deserialize, Serialize};

use crate::risk::{LossLimits, validate_loss_limits, validate_max_notional};
use crate::strategy::{TrackConfig, validate_config};

pub const DEFAULT_TICK_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackId(String);

impl TrackId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TrackId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for TrackId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Binance,
    Bybit,
    Hyperliquid,
    Okx,
}

impl Venue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Bybit => "bybit",
            Self::Hyperliquid => "hyperliquid",
            Self::Okx => "okx",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Instrument {
    pub venue: Venue,
    pub symbol: String,
}

impl Instrument {
    pub fn new(venue: Venue, symbol: impl Into<String>) -> Self {
        Self {
            venue,
            symbol: symbol.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackDefinition {
    track_id: TrackId,
    instrument: Instrument,
    track_config: TrackConfig,
    max_notional: f64,
    loss_limits: LossLimits,
    tick_timeout_secs: u64,
}

impl TrackDefinition {
    pub fn try_new(
        track_id: TrackId,
        instrument: Instrument,
        track_config: TrackConfig,
        max_notional: Option<f64>,
        loss_limits: LossLimits,
        tick_timeout_secs: Option<u64>,
    ) -> Result<Self, String> {
        validate_config(&track_config)?;

        let max_notional = max_notional.unwrap_or_else(|| curve_max_notional(&track_config));
        validate_max_notional(max_notional)?;
        validate_loss_limits(&loss_limits)?;

        Ok(Self {
            track_id,
            instrument,
            track_config,
            max_notional,
            loss_limits,
            tick_timeout_secs: tick_timeout_secs.unwrap_or(DEFAULT_TICK_TIMEOUT_SECS),
        })
    }

    pub fn track_id(&self) -> &TrackId {
        &self.track_id
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn track_config(&self) -> &TrackConfig {
        &self.track_config
    }

    pub fn curve_max_notional(&self) -> f64 {
        curve_max_notional(&self.track_config)
    }

    pub fn max_notional(&self) -> f64 {
        self.max_notional
    }

    pub fn effective_max_notional(&self) -> f64 {
        effective_max_notional(&self.track_config, self.max_notional)
    }

    pub fn loss_limits(&self) -> &LossLimits {
        &self.loss_limits
    }

    pub fn tick_timeout_secs(&self) -> u64 {
        self.tick_timeout_secs
    }
}

pub fn curve_max_notional(config: &TrackConfig) -> f64 {
    config.long_exposure_units.max(config.short_exposure_units) * config.notional_per_unit
}

pub fn effective_max_notional(config: &TrackConfig, max_notional: f64) -> f64 {
    curve_max_notional(config).min(max_notional)
}

#[cfg(test)]
mod tests {
    use crate::risk::LossLimits;
    use crate::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};

    use super::{Instrument, TrackDefinition, TrackId, Venue};

    fn track_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        }
    }

    fn loss_limits() -> LossLimits {
        LossLimits {
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
        }
    }

    fn track_definition(max_notional: Option<f64>) -> TrackDefinition {
        TrackDefinition::try_new(
            TrackId::new("btc-core"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            track_config(),
            max_notional,
            loss_limits(),
            None,
        )
        .unwrap()
    }

    #[test]
    fn venue_as_str_supports_bybit() {
        assert_eq!(Venue::Bybit.as_str(), "bybit");
    }

    #[test]
    fn venue_as_str_supports_hyperliquid() {
        assert_eq!(Venue::Hyperliquid.as_str(), "hyperliquid");
    }

    #[test]
    fn venue_as_str_supports_okx() {
        assert_eq!(Venue::Okx.as_str(), "okx");
    }

    #[test]
    fn track_definition_validates_and_expands_defaults() {
        let definition = TrackDefinition::try_new(
            TrackId::new("btc-core"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            track_config(),
            None,
            loss_limits(),
            None,
        )
        .unwrap();

        assert_eq!(definition.max_notional(), 3_000.0);
        assert_eq!(definition.tick_timeout_secs(), 30);
    }

    #[test]
    fn track_definition_derives_effective_max_notional_from_curve_and_limit() {
        let definition = track_definition(Some(2_000.0));

        assert_eq!(definition.curve_max_notional(), 3_000.0);
        assert_eq!(definition.max_notional(), 2_000.0);
        assert_eq!(definition.effective_max_notional(), 2_000.0);
    }
}
