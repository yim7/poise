use chrono::{DateTime, Utc};
use poise_core::events::ReplacementGateReason;
use poise_core::types::Exposure;
use poise_engine::ledger::TrackLedgerState;
use poise_engine::price_gate::PriceExecutionBlockReason;
use poise_engine::runtime::{ExecutorState, StrategyPriceStatus, TrackStatus};
use poise_engine::snapshot::TrackRuntimeSnapshot;

use crate::track_definition::TrackReadDefinition;
use crate::track_persistence::{PersistedTrackEffect, StoredTrackEvent};

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRuntimeReadState {
    pub status: TrackStatus,
    pub current_exposure: Exposure,
    pub desired_exposure: Option<Exposure>,
    pub manual_target_override: Option<Exposure>,
    pub executor_state: ExecutorState,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub ledger_state: TrackLedgerState,
    pub unrealized_pnl: f64,
    pub has_account_margin_guard: bool,
    pub price_execution_block_reason: Option<PriceExecutionBlockReason>,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub market_data_stale_since: Option<DateTime<Utc>>,
}

impl TrackRuntimeReadState {
    pub fn from_snapshot(snapshot: TrackRuntimeSnapshot) -> Self {
        let TrackRuntimeSnapshot {
            status,
            current_exposure,
            desired_exposure,
            manual_target_override,
            executor_state,
            replacement_gate_reason,
            price_execution_block_reason,
            ledger_state,
            risk,
            observed,
            ..
        } = snapshot;

        Self {
            status,
            current_exposure,
            desired_exposure,
            manual_target_override,
            executor_state,
            replacement_gate_reason,
            ledger_state,
            unrealized_pnl: risk.unrealized_pnl,
            has_account_margin_guard: risk.account_capacity_constraint.increase_blocked,
            price_execution_block_reason,
            strategy_price: observed.strategy_price,
            strategy_price_status: observed.strategy_price_status,
            mark_price: observed.mark_price,
            best_bid: observed.best_bid,
            best_ask: observed.best_ask,
            market_data_stale_since: observed.market_data_stale_since,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadSource {
    pub definition: TrackReadDefinition,
    pub runtime: TrackRuntimeReadState,
    pub updated_at: DateTime<Utc>,
    pub recent_track_events: Vec<StoredTrackEvent>,
    pub recent_effects: Vec<PersistedTrackEffect>,
}
