use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountRiskSignal {
    #[default]
    Normal,
    Attention,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AccountReadModel {
    pub equity: f64,
    pub available: f64,
    pub unrealized_pnl: f64,
    pub baseline_equity: f64,
    pub day_base_at: DateTime<Utc>,
    pub day_change_pct: Option<f64>,
    pub risk_signal: AccountRiskSignal,
    pub reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}
