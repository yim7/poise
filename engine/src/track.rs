use serde::{Deserialize, Serialize};

use poise_core::risk::LossLimits;
use poise_core::strategy::TrackConfig;

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
}

impl Venue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Bybit => "bybit",
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
    pub id: TrackId,
    pub instrument: Instrument,
    pub config: TrackConfig,
    pub max_notional: f64,
    pub loss_limits: LossLimits,
}

impl TrackDefinition {
    pub fn new(
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        max_notional: f64,
        loss_limits: LossLimits,
    ) -> Self {
        Self {
            id,
            instrument,
            config,
            max_notional,
            loss_limits,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Venue;

    #[test]
    fn venue_as_str_supports_bybit() {
        assert_eq!(Venue::Bybit.as_str(), "bybit");
    }
}
