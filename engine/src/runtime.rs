use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use grid_core::strategy::GridConfig;
use grid_core::types::{ExchangeRules, Exposure, Side};

use crate::grid::{GridId, Instrument};
use crate::ports::OrderStatus;
use crate::snapshot::{GridRuntimeSnapshot, ObservedState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridStatus {
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
    pub order_id: Option<String>,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub target_exposure: Exposure,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub realized_pnl_today: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone)]
pub struct GridRuntime {
    pub id: GridId,
    pub instrument: Instrument,
    pub config: GridConfig,
    pub exchange_rules: ExchangeRules,
    pub status: GridStatus,
    pub current_exposure: Exposure,
    // Reconcile owns target_exposure; exchange sync/restore own observed order and risk fields.
    pub target_exposure: Option<Exposure>,
    pub pending_order: Option<PendingOrder>,
    pub risk_state: RiskState,
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

impl GridRuntime {
    pub fn new(
        id: GridId,
        instrument: Instrument,
        config: GridConfig,
        exchange_rules: ExchangeRules,
    ) -> Self {
        Self {
            id,
            instrument,
            config,
            exchange_rules,
            status: GridStatus::WaitingMarketData,
            current_exposure: Exposure(0.0),
            target_exposure: None,
            pending_order: None,
            risk_state: RiskState::default(),
            reference_price: None,
            out_of_band_since: None,
        }
    }

    pub fn symbol(&self) -> &str {
        &self.instrument.symbol
    }

    pub fn snapshot(&self) -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: self.id.clone(),
            instrument: self.instrument.clone(),
            config: self.config.clone(),
            status: self.status.clone(),
            current_exposure: self.current_exposure.clone(),
            target_exposure: self.target_exposure.clone(),
            pending_order: self.pending_order.clone(),
            risk: self.risk_state.clone(),
            observed: ObservedState {
                reference_price: self.reference_price,
                out_of_band_since: self.out_of_band_since,
            },
        }
    }

    pub fn restore(snapshot: GridRuntimeSnapshot, exchange_rules: ExchangeRules) -> Result<Self> {
        Ok(Self {
            id: snapshot.grid_id,
            instrument: snapshot.instrument,
            config: snapshot.config,
            exchange_rules,
            status: snapshot.status,
            current_exposure: snapshot.current_exposure,
            target_exposure: snapshot.target_exposure,
            pending_order: snapshot.pending_order,
            risk_state: snapshot.risk,
            reference_price: snapshot.observed.reference_price,
            out_of_band_since: snapshot.observed.out_of_band_since,
        })
    }
}
