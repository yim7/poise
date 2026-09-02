use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use poise_core::instrument::Asset;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountRiskSignal {
    #[default]
    Normal,
    Attention,
    Critical,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountReadModel {
    pub valuation_asset: Asset,
    pub equity: f64,
    pub available: Option<f64>,
    pub available_by_asset: BTreeMap<Asset, f64>,
    pub unrealized_pnl: Option<f64>,
    pub baseline_equity: f64,
    pub day_base_at: DateTime<Utc>,
    pub day_change_pct: Option<f64>,
    pub risk_signal: AccountRiskSignal,
    pub reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}
