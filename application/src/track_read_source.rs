use chrono::{DateTime, Utc};
use poise_core::events::ExecutionGateReason;
use poise_core::types::Exposure;
use poise_engine::execution_gate::ExecutionGateDecision;
use poise_engine::ledger::TrackLedgerState;
use poise_engine::price_gate::PriceExecutionBlockReason;
use poise_engine::runtime::{ExecutorState, StrategyPriceStatus, TrackLiveView, TrackStatus};
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
    pub fn from_parts(snapshot: TrackRuntimeSnapshot, live: TrackLiveView) -> Self {
        let TrackRuntimeSnapshot {
            runtime_state,
            current_exposure,
            desired_exposure,
            executor_state,
            execution_gate_state,
            ledger_state,
            risk,
            observed,
            ..
        } = snapshot;

        Self {
            status: runtime_state.status(),
            current_exposure,
            desired_exposure: live.desired_exposure.map(Exposure).or(desired_exposure),
            manual_target_override: runtime_state.manual_target_override(),
            executor_state,
            ledger_state,
            unrealized_pnl: risk.unrealized_pnl,
            has_account_margin_guard: matches!(
                execution_gate_state.last_decision,
                ExecutionGateDecision::NoSubmit {
                    reason: ExecutionGateReason::AccountCapacityInsufficient { .. },
                }
            ),
            price_execution_block_reason: live
                .price_execution_block_reason
                .or(execution_gate_state.price_execution_block_reason),
            strategy_price: live.strategy_price,
            strategy_price_status: live.strategy_price_status,
            mark_price: live.mark_price,
            best_bid: live.best_bid,
            best_ask: live.best_ask,
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::strategy::{BandBoundary, BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::Exposure;
    use poise_engine::runtime::{
        AutoState, ControlState, ExecutorState, ManualState, RiskState, TrackLiveView, TrackState,
    };
    use poise_engine::snapshot::TrackRestoreRevision;
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};

    use super::TrackRuntimeReadState;

    #[test]
    fn read_source_derives_manual_flattening_status_from_runtime_state() {
        let snapshot = test_snapshot_with_runtime_state(TrackState::Running(ControlState::Manual(
            ManualState::Flattened,
        )));

        let source = TrackRuntimeReadState::from_parts(snapshot, TrackLiveView::default());

        assert_eq!(
            source.status,
            poise_engine::runtime::TrackStatus::ManualFlattening
        );
    }

    #[test]
    fn flatten_pending_projects_as_frozen_without_leaking_private_state() {
        let snapshot = test_snapshot_with_runtime_state(TrackState::Running(
            ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(4.0),
                boundary: BandBoundary::Below,
            }),
        ));

        let source = TrackRuntimeReadState::from_parts(snapshot, TrackLiveView::default());

        assert_eq!(source.status, poise_engine::runtime::TrackStatus::Frozen);
    }

    fn test_snapshot_with_runtime_state(runtime_state: TrackState) -> TrackRuntimeSnapshot {
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        let config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        };

        TrackRuntimeSnapshot {
            track_id: TrackId::new("btc-core"),
            restore_revision: TrackRestoreRevision::for_track(&instrument, &config),
            runtime_state,
            current_exposure: Exposure(0.0),
            desired_exposure: Some(Exposure(0.0)),
            executor_state: ExecutorState::empty(Utc::now()),
            execution_gate_state: poise_engine::execution_gate::ExecutionGateState::open(),
            ledger_state: Default::default(),
            risk: RiskState::default(),
            observed: ObservedState::default(),
        }
    }
}
