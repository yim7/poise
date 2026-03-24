use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use grid_core::strategy::GridConfig;
use grid_core::types::{ExchangeRules, Exposure, Side};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingOrder {
    pub symbol: String,
    pub order_id: Option<String>,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub target_exposure: Exposure,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub realized_pnl_today: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone)]
pub struct StrategyInstance {
    pub id: String,
    pub symbol: String,
    pub config: GridConfig,
    pub exchange_rules: ExchangeRules,
    pub status: InstanceStatus,
    pub current_exposure: Exposure,
    // Reconcile owns target_exposure; exchange sync/restore own observed order and risk fields.
    pub target_exposure: Option<Exposure>,
    pub pending_order: Option<PendingOrder>,
    pub risk_state: RiskState,
    pub last_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

impl StrategyInstance {
    pub fn new(
        id: String,
        symbol: String,
        config: GridConfig,
        exchange_rules: ExchangeRules,
    ) -> Self {
        Self {
            id,
            symbol,
            config,
            exchange_rules,
            status: InstanceStatus::WaitingMarketData,
            current_exposure: Exposure(0.0),
            target_exposure: None,
            pending_order: None,
            risk_state: RiskState::default(),
            last_price: None,
            out_of_band_since: None,
        }
    }
}
