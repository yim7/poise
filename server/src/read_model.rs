use chrono::{DateTime, Utc};
use poise_core::events::ReplacementGateReason;
use poise_core::strategy::{OutOfBandPolicy, ShapeFamily};
use poise_core::types::Side;
use poise_engine::executor::{ExecutionMode, OrderRole};
use poise_engine::ports::{PersistedTrackEffect, StoredTrackEvent};
use poise_engine::runtime::{SlotState, TrackStatus};
use poise_engine::snapshot::TrackRuntimeSnapshot;

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadModel {
    pub track_id: String,
    pub venue: String,
    pub symbol: String,
    pub status: TrackStatus,
    pub updated_at: DateTime<Utc>,
    pub lower_price: f64,
    pub upper_price: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
    pub reference_price: Option<f64>,
    pub current_exposure: f64,
    pub target_exposure: Option<f64>,
    pub realized_pnl_cumulative: f64,
    pub unrealized_pnl: f64,
    pub executor_mode: ExecutionMode,
    pub inventory_gap: f64,
    pub gap_started_at: Option<DateTime<Utc>>,
    pub max_inventory_gap_abs: f64,
    pub max_gap_age_ms: i64,
    pub stats_started_at: DateTime<Utc>,
    pub has_recovery_anomaly: bool,
    pub has_stale_market_data: bool,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub slots: Vec<ReadModelSlot>,
    pub manual_target_override: Option<f64>,
    pub recent_track_events: Vec<StoredTrackEvent>,
    pub recent_effects: Vec<PersistedTrackEffect>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadModelSlot {
    pub label: String,
    pub is_submit_pending: bool,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub role: OrderRole,
}

impl TrackReadModel {
    pub fn from_snapshot(
        snapshot: TrackRuntimeSnapshot,
        updated_at: DateTime<Utc>,
        recent_track_events: Vec<StoredTrackEvent>,
        recent_effects: Vec<PersistedTrackEffect>,
    ) -> Self {
        let TrackRuntimeSnapshot {
            track_id,
            instrument,
            config,
            status,
            current_exposure,
            target_exposure,
            manual_target_override,
            executor_state,
            replacement_gate_reason,
            risk,
            observed,
        } = snapshot;

        let slots = executor_state
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let order = slot.working_order.as_ref()?;
                Some(ReadModelSlot {
                    label: project_slot_label(index, slot.slot.0.as_str()),
                    is_submit_pending: matches!(slot.state, SlotState::SubmitPending),
                    side: order.side,
                    price: order.price,
                    quantity: order.quantity,
                    role: order.role.clone(),
                })
            })
            .collect();

        Self {
            track_id: track_id.as_str().to_string(),
            venue: instrument.venue.as_str().to_string(),
            symbol: instrument.symbol,
            status,
            updated_at,
            lower_price: config.lower_price,
            upper_price: config.upper_price,
            shape_family: config.shape_family,
            out_of_band_policy: config.out_of_band_policy,
            reference_price: observed.reference_price,
            current_exposure: current_exposure.0,
            target_exposure: target_exposure.map(|value| value.0),
            realized_pnl_cumulative: risk.realized_pnl_cumulative,
            unrealized_pnl: risk.unrealized_pnl,
            executor_mode: executor_state.mode,
            inventory_gap: executor_state.inventory_gap.0,
            gap_started_at: executor_state.gap_started_at,
            max_inventory_gap_abs: executor_state.stats.max_inventory_gap_abs.0,
            max_gap_age_ms: executor_state.stats.max_gap_age_ms,
            stats_started_at: executor_state.stats.started_at,
            has_recovery_anomaly: executor_state.recovery_anomaly.is_some(),
            has_stale_market_data: observed.market_data_stale_since.is_some(),
            replacement_gate_reason,
            slots,
            manual_target_override: manual_target_override.map(|value| value.0),
            recent_track_events,
            recent_effects,
        }
    }
}

fn project_slot_label(index: usize, slot_name: &str) -> String {
    match slot_name {
        "inventory_core" => "inventory".to_string(),
        _ => format!("slot {}", index + 1),
    }
}
