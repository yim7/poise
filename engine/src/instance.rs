use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use grid_core::strategy::GridConfig;
use grid_core::types::Exposure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InstanceStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}

#[derive(Debug, Clone)]
pub struct StrategyInstance {
    pub id: String,
    pub symbol: String,
    pub config: GridConfig,
    pub status: InstanceStatus,
    pub current_exposure: Exposure,
    pub last_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

impl StrategyInstance {
    pub fn new(id: String, symbol: String, config: GridConfig) -> Self {
        Self {
            id,
            symbol,
            config,
            status: InstanceStatus::WaitingMarketData,
            current_exposure: Exposure(0.0),
            last_price: None,
            out_of_band_since: None,
        }
    }
}
