use chrono::{DateTime, Utc};
use poise_core::events::{DomainEvent, ExecutionGateReason, ReplacementGateReason};
use poise_core::risk::CapacityBudget;
use poise_core::strategy::{BandProtectionPolicy, ShapeFamily};
use poise_core::types::Side;
use poise_engine::executor::{ExecutionMode, OrderRole, RecoveryAnomaly};
use poise_engine::ledger::{LedgerGapReason, LedgerGapRecord, TrackLedgerState};
use poise_engine::price_gate::PriceExecutionBlockReason;
use poise_engine::runtime::{SlotState, StrategyPriceStatus, TrackStatus};
use poise_engine::transition::TrackEffect;
use serde::{Deserialize, Serialize};

use crate::track_definition::TrackReadDefinition;
use crate::track_persistence::{EffectStatus, PersistedTrackEffect, StoredTrackEvent};
use crate::track_read_source::{TrackReadSource, TrackRuntimeReadState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackActivityEntry {
    pub ts: DateTime<Utc>,
    pub message: String,
    pub level: TrackActivityLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackActivityLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackListReadModel {
    pub track_id: String,
    pub venue: String,
    pub symbol: String,
    pub status: TrackReadStatus,
    pub updated_at: DateTime<Utc>,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: TrackStrategyPriceStatus,
    pub current_exposure: f64,
    pub desired_exposure: Option<f64>,
    pub ledger_state: TrackReadLedgerState,
    pub unrealized_pnl: f64,
    pub recovery_issue: Option<TrackRecoveryIssue>,
    pub has_account_margin_guard: bool,
    pub has_stale_market_data: bool,
    pub price_execution_block_reason: Option<TrackPriceExecutionBlockReason>,
    pub active_slot_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadModel {
    pub track_id: String,
    pub venue: String,
    pub symbol: String,
    pub status: TrackReadStatus,
    pub updated_at: DateTime<Utc>,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub min_rebalance_units: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: BandProtectionPolicy,
    pub budget: CapacityBudget,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: TrackStrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub current_exposure: f64,
    pub desired_exposure: Option<f64>,
    pub ledger_state: TrackReadLedgerState,
    pub unrealized_pnl: f64,
    pub executor_mode: TrackReadExecutionMode,
    pub inventory_gap: f64,
    pub gap_started_at: Option<DateTime<Utc>>,
    pub max_inventory_gap_abs: f64,
    pub max_gap_age_ms: i64,
    pub stats_started_at: DateTime<Utc>,
    pub recovery_issue: Option<TrackRecoveryIssue>,
    pub has_account_margin_guard: bool,
    pub has_stale_market_data: bool,
    pub price_execution_block_reason: Option<TrackPriceExecutionBlockReason>,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
    pub slots: Vec<ReadModelSlot>,
    pub manual_target_override: Option<f64>,
    pub recent_activity: Vec<TrackActivityEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadModelSlot {
    pub label: String,
    pub is_submit_pending: bool,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub role: TrackReadOrderRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackReadStatus {
    WaitingMarketData,
    Active,
    Frozen,
    Holding,
    Flattening,
    ManualFlattening,
    Terminated,
    Paused,
}

impl From<TrackStatus> for TrackReadStatus {
    fn from(value: TrackStatus) -> Self {
        match value {
            TrackStatus::WaitingMarketData => Self::WaitingMarketData,
            TrackStatus::Active => Self::Active,
            TrackStatus::Frozen => Self::Frozen,
            TrackStatus::Holding => Self::Holding,
            TrackStatus::Flattening => Self::Flattening,
            TrackStatus::ManualFlattening => Self::ManualFlattening,
            TrackStatus::Terminated => Self::Terminated,
            TrackStatus::Paused => Self::Paused,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackStrategyPriceStatus {
    Live,
    Stale,
}

impl From<StrategyPriceStatus> for TrackStrategyPriceStatus {
    fn from(value: StrategyPriceStatus) -> Self {
        match value {
            StrategyPriceStatus::Live => Self::Live,
            StrategyPriceStatus::Stale => Self::Stale,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackReadExecutionMode {
    Passive,
    Rebalance,
    CatchUp,
}

impl From<ExecutionMode> for TrackReadExecutionMode {
    fn from(value: ExecutionMode) -> Self {
        match value {
            ExecutionMode::Passive => Self::Passive,
            ExecutionMode::Rebalance => Self::Rebalance,
            ExecutionMode::CatchUp => Self::CatchUp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackReadOrderRole {
    IncreaseInventory,
    DecreaseInventory,
}

impl From<OrderRole> for TrackReadOrderRole {
    fn from(value: OrderRole) -> Self {
        match value {
            OrderRole::IncreaseInventory => Self::IncreaseInventory,
            OrderRole::DecreaseInventory => Self::DecreaseInventory,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackRecoveryIssue {
    UnknownLiveOrder,
    DuplicateLiveOrders,
    AmbiguousLiveOrder,
}

impl From<RecoveryAnomaly> for TrackRecoveryIssue {
    fn from(value: RecoveryAnomaly) -> Self {
        match value {
            RecoveryAnomaly::UnknownLiveOrder => Self::UnknownLiveOrder,
            RecoveryAnomaly::DuplicateLiveOrders => Self::DuplicateLiveOrders,
            RecoveryAnomaly::AmbiguousLiveOrder => Self::AmbiguousLiveOrder,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackPriceExecutionBlockReason {
    MissingExecutionQuote,
    MarkBookDivergence,
}

impl From<PriceExecutionBlockReason> for TrackPriceExecutionBlockReason {
    fn from(value: PriceExecutionBlockReason) -> Self {
        match value {
            PriceExecutionBlockReason::MissingExecutionQuote => Self::MissingExecutionQuote,
            PriceExecutionBlockReason::MarkBookDivergence => Self::MarkBookDivergence,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TrackReadLedgerState {
    pub gross_realized_pnl_today: f64,
    pub gross_realized_pnl_cumulative: f64,
    pub trading_fee_today: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_today: f64,
    pub funding_fee_cumulative: f64,
    pub unresolved_gaps: Vec<TrackReadLedgerGap>,
}

impl TrackReadLedgerState {
    pub fn net_realized_pnl(&self) -> f64 {
        self.gross_realized_pnl_cumulative - self.trading_fee_cumulative
            + self.funding_fee_cumulative
    }
}

impl From<TrackLedgerState> for TrackReadLedgerState {
    fn from(value: TrackLedgerState) -> Self {
        Self {
            gross_realized_pnl_today: value.gross_realized_pnl_today,
            gross_realized_pnl_cumulative: value.gross_realized_pnl_cumulative,
            trading_fee_today: value.trading_fee_today,
            trading_fee_cumulative: value.trading_fee_cumulative,
            funding_fee_today: value.funding_fee_today,
            funding_fee_cumulative: value.funding_fee_cumulative,
            unresolved_gaps: value
                .unresolved_gaps
                .into_iter()
                .map(TrackReadLedgerGap::from)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackReadLedgerGap {
    pub gap_key: String,
    pub reason: TrackReadLedgerGapReason,
    pub observed_at: DateTime<Utc>,
}

impl From<LedgerGapRecord> for TrackReadLedgerGap {
    fn from(value: LedgerGapRecord) -> Self {
        Self {
            gap_key: value.gap_key,
            reason: value.reason.into(),
            observed_at: value.observed_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackReadLedgerGapReason {
    UnsupportedCommissionAsset,
    MissingCommissionAsset,
    MissingSymbol,
    UnsupportedFundingAsset,
}

impl From<LedgerGapReason> for TrackReadLedgerGapReason {
    fn from(value: LedgerGapReason) -> Self {
        match value {
            LedgerGapReason::UnsupportedCommissionAsset => Self::UnsupportedCommissionAsset,
            LedgerGapReason::MissingCommissionAsset => Self::MissingCommissionAsset,
            LedgerGapReason::MissingSymbol => Self::MissingSymbol,
            LedgerGapReason::UnsupportedFundingAsset => Self::UnsupportedFundingAsset,
        }
    }
}

impl TrackReadModel {
    pub(crate) fn from_source(source: TrackReadSource) -> Self {
        let TrackReadSource {
            definition,
            runtime,
            updated_at,
            recent_track_events,
            recent_effects,
        } = source;
        let list_view = TrackListReadModel::from_parts(&definition, &runtime, updated_at);
        let replacement_gate_reason = runtime.replacement_gate_reason.clone();
        let recent_activity = project_recent_activity(recent_track_events, recent_effects);

        let slots = runtime
            .executor_state
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
                    role: TrackReadOrderRole::from(order.role.clone()),
                })
            })
            .collect();

        Self {
            track_id: list_view.track_id.clone(),
            venue: list_view.venue.clone(),
            symbol: list_view.symbol.clone(),
            status: list_view.status,
            updated_at: list_view.updated_at,
            lower_price: definition.track_config.lower_price,
            upper_price: definition.track_config.upper_price,
            long_exposure_units: definition.track_config.long_exposure_units,
            short_exposure_units: definition.track_config.short_exposure_units,
            notional_per_unit: definition.track_config.notional_per_unit,
            min_rebalance_units: definition.track_config.min_rebalance_units,
            shape_family: definition.track_config.shape_family,
            out_of_band_policy: definition.track_config.out_of_band_policy,
            budget: definition.budget,
            strategy_price: list_view.strategy_price,
            strategy_price_status: list_view.strategy_price_status,
            mark_price: runtime.mark_price,
            best_bid: runtime.best_bid,
            best_ask: runtime.best_ask,
            current_exposure: list_view.current_exposure,
            desired_exposure: list_view.desired_exposure,
            ledger_state: list_view.ledger_state.clone(),
            unrealized_pnl: list_view.unrealized_pnl,
            executor_mode: TrackReadExecutionMode::from(
                runtime.executor_state.diagnostics.mode.clone(),
            ),
            inventory_gap: runtime.executor_state.diagnostics.inventory_gap.0,
            gap_started_at: runtime.executor_state.diagnostics.gap_started_at,
            max_inventory_gap_abs: runtime.executor_state.stats.max_inventory_gap_abs.0,
            max_gap_age_ms: runtime.executor_state.stats.max_gap_age_ms,
            stats_started_at: runtime.executor_state.stats.started_at,
            recovery_issue: list_view.recovery_issue,
            has_account_margin_guard: list_view.has_account_margin_guard,
            has_stale_market_data: list_view.has_stale_market_data,
            price_execution_block_reason: list_view.price_execution_block_reason,
            replacement_gate_reason,
            slots,
            manual_target_override: runtime.manual_target_override.map(|value| value.0),
            recent_activity,
        }
    }
}

impl TrackListReadModel {
    pub(crate) fn from_parts(
        definition: &TrackReadDefinition,
        runtime: &TrackRuntimeReadState,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            track_id: definition.track_id.as_str().to_string(),
            venue: definition.instrument.venue.as_str().to_string(),
            symbol: definition.instrument.symbol.clone(),
            status: TrackReadStatus::from(runtime.status.clone()),
            updated_at,
            strategy_price: runtime.strategy_price,
            strategy_price_status: TrackStrategyPriceStatus::from(
                runtime.strategy_price_status.clone(),
            ),
            current_exposure: runtime.current_exposure.0,
            desired_exposure: runtime.desired_exposure.clone().map(|value| value.0),
            ledger_state: TrackReadLedgerState::from(runtime.ledger_state.clone()),
            unrealized_pnl: runtime.unrealized_pnl,
            recovery_issue: runtime
                .executor_state
                .diagnostics
                .recovery_anomaly
                .clone()
                .map(TrackRecoveryIssue::from),
            has_account_margin_guard: runtime.has_account_margin_guard,
            has_stale_market_data: runtime.market_data_stale_since.is_some(),
            price_execution_block_reason: runtime
                .price_execution_block_reason
                .map(TrackPriceExecutionBlockReason::from),
            active_slot_count: runtime
                .executor_state
                .slots
                .iter()
                .filter(|slot| slot.working_order.is_some())
                .count() as u32,
        }
    }
}

impl From<&TrackReadModel> for TrackListReadModel {
    fn from(value: &TrackReadModel) -> Self {
        Self {
            track_id: value.track_id.clone(),
            venue: value.venue.clone(),
            symbol: value.symbol.clone(),
            status: value.status,
            updated_at: value.updated_at,
            strategy_price: value.strategy_price,
            strategy_price_status: value.strategy_price_status,
            current_exposure: value.current_exposure,
            desired_exposure: value.desired_exposure,
            ledger_state: value.ledger_state.clone(),
            unrealized_pnl: value.unrealized_pnl,
            recovery_issue: value.recovery_issue,
            has_account_margin_guard: value.has_account_margin_guard,
            has_stale_market_data: value.has_stale_market_data,
            price_execution_block_reason: value.price_execution_block_reason,
            active_slot_count: value.slots.len() as u32,
        }
    }
}

fn project_recent_activity(
    recent_track_events: Vec<StoredTrackEvent>,
    recent_effects: Vec<PersistedTrackEffect>,
) -> Vec<TrackActivityEntry> {
    let mut items = Vec::new();

    for event in recent_track_events {
        if matches!(event.event, DomainEvent::ExposureTargetChanged { .. }) {
            continue;
        }

        items.push(TrackActivityEntry {
            ts: event.created_at,
            message: project_domain_event_message(&event.event),
            level: project_domain_event_level(&event.event),
        });
    }

    for effect in recent_effects {
        items.push(TrackActivityEntry {
            ts: effect.updated_at,
            message: project_effect_message(&effect),
            level: project_effect_level(effect.status),
        });
    }

    items.sort_by_key(|item| item.ts);
    items
}

fn project_domain_event_message(event: &DomainEvent) -> String {
    match event {
        DomainEvent::ExposureTargetChanged { from, to } => {
            format!("desired exposure {:.4} -> {:.4}", from.0, to.0)
        }
        DomainEvent::BandBreached { boundary, price } => {
            format!("band breached {:?} at {:.4}", boundary, price)
        }
        DomainEvent::BandReentered { price } => format!("band reentered at {:.4}", price),
        DomainEvent::PolicyTriggered { policy } => format!("policy triggered: {:?}", policy),
        DomainEvent::RiskCapApplied { intended, capped } => {
            format!("risk cap {:.4} -> {:.4}", intended.0, capped.0)
        }
        DomainEvent::ExecutionGateApplied { reason } => match reason {
            ExecutionGateReason::AccountCapacityInsufficient {
                required_notional,
                available_notional,
            } => format!(
                "execution gate: account capacity insufficient {:.4} > {:.4}",
                required_notional, available_notional
            ),
        },
        DomainEvent::ReplacementGateApplied { reason } => match reason {
            ReplacementGateReason::RoundedMatch => {
                "replacement gate: candidate matches working order after rounding".into()
            }
            ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps,
                threshold_bps,
            } => format!(
                "replacement gate: improvement {:.1} bps < threshold {:.1} bps",
                improvement_bps, threshold_bps
            ),
        },
    }
}

fn project_domain_event_level(event: &DomainEvent) -> TrackActivityLevel {
    match event {
        DomainEvent::ExecutionGateApplied { .. } => TrackActivityLevel::Warn,
        _ => TrackActivityLevel::Info,
    }
}

fn project_effect_message(effect: &PersistedTrackEffect) -> String {
    match &effect.effect {
        TrackEffect::SubmitOrder { .. } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| "submit order failed".into()),
            EffectStatus::Succeeded => "submit order succeeded".into(),
            EffectStatus::Superseded => "submit order superseded by newer track state".into(),
            EffectStatus::Executing => "submit order executing".into(),
            EffectStatus::Pending => "submit order pending".into(),
        },
        TrackEffect::CancelOrder { order_id, .. } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| format!("cancel {order_id} failed")),
            EffectStatus::Succeeded => format!("cancel {order_id} succeeded"),
            EffectStatus::Superseded => format!("cancel {order_id} superseded"),
            EffectStatus::Executing => format!("cancel {order_id} executing"),
            EffectStatus::Pending => format!("cancel {order_id} pending"),
        },
        TrackEffect::CancelAll { instrument } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| format!("cancel all {} failed", instrument.symbol)),
            EffectStatus::Succeeded => format!("cancel all {} succeeded", instrument.symbol),
            EffectStatus::Superseded => format!("cancel all {} superseded", instrument.symbol),
            EffectStatus::Executing => format!("cancel all {} executing", instrument.symbol),
            EffectStatus::Pending => format!("cancel all {} pending", instrument.symbol),
        },
        TrackEffect::NoOp => "no-op".into(),
    }
}

fn project_effect_level(status: EffectStatus) -> TrackActivityLevel {
    match status {
        EffectStatus::Failed => TrackActivityLevel::Error,
        _ => TrackActivityLevel::Info,
    }
}

fn project_slot_label(index: usize, slot_name: &str) -> String {
    match slot_name {
        "inventory_core" => "inventory".to_string(),
        _ => format!("slot {}", index + 1),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use poise_core::events::{DomainEvent, ReplacementGateReason};
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{BandProtectionPolicy, BandRecoverPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{Exposure, Side};
    use poise_engine::executor::{ExecutionMode, OrderRole, OrderSlot};
    use poise_engine::persisted_runtime::TrackRestoreRevision;
    use poise_engine::ports::{OrderRequest, OrderStatus};
    use poise_engine::runtime::{
        AutoState, ControlState, ExecutionSlot, ExecutionStats, ExecutorState, RiskState,
        SlotState, StrategyPriceStatus, TrackLiveView, TrackState, TrackStatus, WorkingOrder,
    };
    use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;

    use super::{
        TrackActivityLevel, TrackPriceExecutionBlockReason, TrackReadExecutionMode, TrackReadModel,
        TrackReadOrderRole, TrackReadStatus, TrackStrategyPriceStatus,
    };
    use crate::TrackReadDefinition;
    use crate::track_persistence::{EffectStatus, PersistedTrackEffect, StoredTrackEvent};
    use crate::track_read_source::{TrackReadSource, TrackRuntimeReadState};

    fn test_track_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
        }
    }

    fn active_runtime_state() -> TrackState {
        TrackState::Running(ControlState::Automatic(AutoState::FollowingBand))
    }

    #[test]
    fn read_model_from_snapshot_flattens_runtime_state() {
        let track_config = test_track_config();
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: TrackReadDefinition {
                track_id: TrackId::new("btc-core"),
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                track_config: track_config.clone(),
                budget: CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: 100.0,
                    total_loss_limit: 300.0,
                },
            },
            runtime: TrackRuntimeReadState::from_snapshot(TrackRuntimeSnapshot {
                track_id: TrackId::new("btc-core"),
                restore_revision: TrackRestoreRevision::for_track(
                    &Instrument::new(Venue::Binance, "BTCUSDT"),
                    &track_config,
                ),
                runtime_state: active_runtime_state(),
                current_exposure: Exposure(3.5),
                desired_exposure: Some(Exposure(4.0)),
                executor_state: ExecutorState {
                    active_round: Some(poise_engine::runtime::ExecutionRound {
                        desired_exposure: Exposure(4.0),
                        mode: ExecutionMode::Passive,
                        started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
                    }),
                    diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                        mode: ExecutionMode::Passive,
                        inventory_gap: Exposure(0.5),
                        gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 26, 10, 0, 0).unwrap()),
                        last_reprice_at: None,
                        last_execution_reason: None,
                        recovery_anomaly: None,
                    },
                    slots: vec![ExecutionSlot {
                        slot: OrderSlot::new("inventory_core"),
                        state: SlotState::Working,
                        working_order: Some(WorkingOrder {
                            order_id: Some("order-1".into()),
                            client_order_id: "client-1".into(),
                            side: Side::Buy,
                            price: 100.5,
                            quantity: 0.1,
                            status: OrderStatus::New,
                            role: OrderRole::IncreaseInventory,
                        }),
                    }],
                    recent_terminal_orders: Vec::new(),
                    stats: ExecutionStats {
                        started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
                        max_inventory_gap_abs: Exposure(0.5),
                        max_gap_age_ms: 0,
                    },
                },
                ledger_state: Default::default(),
                replacement_gate_reason: None,
                execution_gate_state: poise_engine::execution_gate::ExecutionGateState::open(),
                risk: RiskState {
                    unrealized_pnl: 0.0,
                    ..RiskState::default()
                },
                observed: ObservedState {
                    strategy_price: Some(101.25),
                    strategy_price_status: StrategyPriceStatus::Live,
                    mark_price: Some(101.5),
                    best_bid: Some(101.0),
                    best_ask: Some(101.5),
                    out_of_band_since: None,
                    last_tick_at: None,
                    market_data_stale_since: None,
                },
            }),
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            recent_track_events: vec![StoredTrackEvent {
                id: 1,
                track_id: TrackId::new("btc-core"),
                event: DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                },
                created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
            }],
            recent_effects: vec![PersistedTrackEffect {
                effect_id: "btc-core:batch-1:0".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "batch-1".into(),
                sequence: 0,
                effect: TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        side: Side::Buy,
                        price: 100.5,
                        quantity: 0.1,
                        client_order_id: "client-1".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(4.0),
                    submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                },
                status: EffectStatus::Executing,
                attempt_count: 0,
                last_error: None,
                created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            }],
        });

        assert_eq!(read_model.track_id, "btc-core");
        assert_eq!(read_model.symbol, "BTCUSDT");
        assert_eq!(read_model.status, TrackReadStatus::Active);
        assert_eq!(read_model.executor_mode, TrackReadExecutionMode::Passive);
        assert_eq!(read_model.recovery_issue, None);
        assert_eq!(
            read_model.slots[0].role,
            TrackReadOrderRole::IncreaseInventory
        );
        assert_eq!(read_model.recent_activity.len(), 1);
        assert_eq!(
            read_model.recent_activity[0].level,
            TrackActivityLevel::Info
        );
        assert_eq!(
            read_model.recent_activity[0].message,
            "submit order executing"
        );
    }

    #[test]
    fn read_model_projects_replacement_gate_event_into_recent_activity() {
        let track_config = test_track_config();
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: TrackReadDefinition {
                track_id: TrackId::new("btc-core"),
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                track_config: track_config.clone(),
                budget: CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: 100.0,
                    total_loss_limit: 300.0,
                },
            },
            runtime: TrackRuntimeReadState::from_snapshot(TrackRuntimeSnapshot {
                track_id: TrackId::new("btc-core"),
                restore_revision: TrackRestoreRevision::for_track(
                    &Instrument::new(Venue::Binance, "BTCUSDT"),
                    &track_config,
                ),
                runtime_state: active_runtime_state(),
                current_exposure: Exposure(3.5),
                desired_exposure: Some(Exposure(4.0)),
                executor_state: ExecutorState::empty(
                    Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
                ),
                ledger_state: Default::default(),
                replacement_gate_reason: None,
                execution_gate_state: poise_engine::execution_gate::ExecutionGateState::open(),
                risk: RiskState::default(),
                observed: ObservedState {
                    strategy_price: Some(101.25),
                    strategy_price_status: StrategyPriceStatus::Live,
                    mark_price: Some(101.5),
                    best_bid: Some(101.0),
                    best_ask: Some(101.5),
                    out_of_band_since: None,
                    last_tick_at: None,
                    market_data_stale_since: None,
                },
            }),
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            recent_track_events: vec![StoredTrackEvent {
                id: 1,
                track_id: TrackId::new("btc-core"),
                event: DomainEvent::ReplacementGateApplied {
                    reason: ReplacementGateReason::RoundedMatch,
                },
                created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
            }],
            recent_effects: Vec::new(),
        });

        assert_eq!(read_model.recent_activity.len(), 1);
        assert_eq!(
            read_model.recent_activity[0].level,
            TrackActivityLevel::Info
        );
        assert_eq!(
            read_model.recent_activity[0].message,
            "replacement gate: candidate matches working order after rounding"
        );
    }

    #[test]
    fn read_model_exposes_strategy_price_status_and_best_bid_ask() {
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: TrackReadDefinition {
                track_id: TrackId::new("btc-core"),
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                track_config: test_track_config(),
                budget: CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: 100.0,
                    total_loss_limit: 300.0,
                },
            },
            runtime: TrackRuntimeReadState {
                status: TrackStatus::Active,
                current_exposure: Exposure(1.0),
                desired_exposure: Some(Exposure(2.0)),
                manual_target_override: None,
                executor_state: ExecutorState {
                    active_round: None,
                    diagnostics: poise_engine::runtime::ExecutorDiagnostics::empty(),
                    slots: Vec::new(),
                    recent_terminal_orders: Vec::new(),
                    stats: ExecutionStats {
                        started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
                        max_inventory_gap_abs: Exposure(0.0),
                        max_gap_age_ms: 0,
                    },
                },
                replacement_gate_reason: None,
                ledger_state: Default::default(),
                unrealized_pnl: 0.0,
                has_account_margin_guard: false,
                price_execution_block_reason: Some(
                    poise_engine::price_gate::PriceExecutionBlockReason::MissingExecutionQuote,
                ),
                strategy_price: Some(101.25),
                strategy_price_status: StrategyPriceStatus::Stale,
                mark_price: Some(101.5),
                best_bid: Some(101.0),
                best_ask: Some(101.5),
                market_data_stale_since: None,
            },
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            recent_track_events: Vec::new(),
            recent_effects: Vec::new(),
        });

        assert_eq!(read_model.strategy_price, Some(101.25));
        assert_eq!(
            read_model.strategy_price_status,
            TrackStrategyPriceStatus::Stale
        );
        assert_eq!(read_model.mark_price, Some(101.5));
        assert_eq!(read_model.best_bid, Some(101.0));
        assert_eq!(read_model.best_ask, Some(101.5));
        assert_eq!(
            read_model.price_execution_block_reason,
            Some(TrackPriceExecutionBlockReason::MissingExecutionQuote)
        );
    }

    #[test]
    fn read_model_uses_track_live_view_for_market_and_target_fields() {
        let track_config = test_track_config();
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: TrackReadDefinition {
                track_id: TrackId::new("btc-core"),
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                track_config: track_config.clone(),
                budget: CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: 100.0,
                    total_loss_limit: 300.0,
                },
            },
            runtime: TrackRuntimeReadState::from_parts(
                TrackRuntimeSnapshot {
                    track_id: TrackId::new("btc-core"),
                    restore_revision: TrackRestoreRevision::for_track(
                        &Instrument::new(Venue::Binance, "BTCUSDT"),
                        &track_config,
                    ),
                    runtime_state: active_runtime_state(),
                    current_exposure: Exposure(1.0),
                    desired_exposure: Some(Exposure(4.0)),
                    executor_state: ExecutorState {
                        active_round: None,
                        diagnostics: poise_engine::runtime::ExecutorDiagnostics::empty(),
                        slots: Vec::new(),
                        recent_terminal_orders: Vec::new(),
                        stats: ExecutionStats {
                            started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
                            max_inventory_gap_abs: Exposure(0.0),
                            max_gap_age_ms: 0,
                        },
                    },
                    ledger_state: Default::default(),
                    replacement_gate_reason: None,
                    execution_gate_state: poise_engine::execution_gate::ExecutionGateState::open(),
                    risk: RiskState {
                        unrealized_pnl: 0.0,
                        ..RiskState::default()
                    },
                    observed: ObservedState {
                        strategy_price: None,
                        strategy_price_status: StrategyPriceStatus::Stale,
                        mark_price: None,
                        best_bid: None,
                        best_ask: None,
                        out_of_band_since: None,
                        last_tick_at: None,
                        market_data_stale_since: None,
                    },
                },
                TrackLiveView {
                    strategy_price: Some(101.25),
                    strategy_price_status: StrategyPriceStatus::Live,
                    mark_price: Some(101.5),
                    best_bid: Some(101.0),
                    best_ask: Some(101.5),
                    desired_exposure: Some(2.0),
                    price_execution_block_reason: Some(
                        poise_engine::price_gate::PriceExecutionBlockReason::MissingExecutionQuote,
                    ),
                },
            ),
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            recent_track_events: Vec::new(),
            recent_effects: Vec::new(),
        });

        assert_eq!(read_model.strategy_price, Some(101.25));
        assert_eq!(
            read_model.strategy_price_status,
            TrackStrategyPriceStatus::Live
        );
        assert_eq!(read_model.mark_price, Some(101.5));
        assert_eq!(read_model.best_bid, Some(101.0));
        assert_eq!(read_model.best_ask, Some(101.5));
        assert_eq!(read_model.desired_exposure, Some(2.0));
        assert_eq!(
            read_model.price_execution_block_reason,
            Some(TrackPriceExecutionBlockReason::MissingExecutionQuote)
        );
    }
}
