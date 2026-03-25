use serde::{Deserialize, Serialize};

use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridId(String);

impl GridId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for GridId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for GridId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Binance,
}

impl Venue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
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
pub struct GridDefinition {
    pub id: GridId,
    pub instrument: Instrument,
    pub config: GridConfig,
    pub budget: CapacityBudget,
}

impl GridDefinition {
    pub fn new(
        id: GridId,
        instrument: Instrument,
        config: GridConfig,
        budget: CapacityBudget,
    ) -> Self {
        Self {
            id,
            instrument,
            config,
            budget,
        }
    }
}
