use chrono::{DateTime, Utc};
use poise_core::events::{DomainEvent, ExecutionGateReason};
use poise_core::risk::LossLimits;
use poise_core::strategy::{BandProtectionPolicy, RiskAcquisitionConfig, ShapeFamily};
use poise_core::track::{Instrument, TrackDefinition};
use poise_core::types::Side;
use poise_engine::execution_plan::TrackEffect;
use poise_engine::executor::{BindingStatus, PolicyKind, RecoveryAnomaly};
use poise_engine::ledger::TrackPnlStats;
use poise_engine::price_gate::PriceExecutionBlockReason;
use poise_engine::runtime::{
    RiskAcquisitionDirection, RiskAcquisitionRuntimeView, StrategyPriceStatus, TrackRuntimeView,
    TrackStatus,
};
use serde::{Deserialize, Serialize};

use crate::track_persistence::{EffectStatus, PersistedTrackEffect, StoredTrackEvent};
use crate::track_read_source::TrackReadSource;

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
    pub instrument: Instrument,
    pub status: TrackReadStatus,
    pub updated_at: DateTime<Utc>,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: TrackStrategyPriceStatus,
    pub current_exposure: f64,
    pub position_qty: f64,
    pub desired_exposure: Option<f64>,
    pub risk_acquisition: Option<TrackRiskAcquisitionReadModel>,
    pub pnl_stats: TrackReadPnlStats,
    pub unrealized_pnl: f64,
    pub recovery_issue: Option<TrackRecoveryIssue>,
    pub has_account_margin_guard: bool,
    pub has_stale_market_data: bool,
    pub price_execution_block_reason: Option<TrackPriceExecutionBlockReason>,
    pub active_binding_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadModel {
    pub track_id: String,
    pub instrument: Instrument,
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
    pub risk_acquisition_config: RiskAcquisitionConfig,
    pub max_notional: f64,
    pub loss_limits: LossLimits,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: TrackStrategyPriceStatus,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub current_exposure: f64,
    pub position_qty: f64,
    pub desired_exposure: Option<f64>,
    pub risk_acquisition: Option<TrackRiskAcquisitionReadModel>,
    pub pnl_stats: TrackReadPnlStats,
    pub unrealized_pnl: f64,
    pub inventory_gap: f64,
    pub recovery_issue: Option<TrackRecoveryIssue>,
    pub has_account_margin_guard: bool,
    pub has_stale_market_data: bool,
    pub price_execution_block_reason: Option<TrackPriceExecutionBlockReason>,
    pub active_binding_count: u32,
    pub bindings: Vec<ReadModelBinding>,
    pub manual_target_override: Option<f64>,
    pub recent_activity: Vec<TrackActivityEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadModelBinding {
    pub id: String,
    pub policy: TrackReadBindingPolicy,
    pub label: String,
    pub status: TrackReadBindingStatus,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub intent: TrackReadBindingIntent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRiskAcquisitionReadModel {
    pub direction: TrackRiskAcquisitionDirection,
    pub curve_target: f64,
    pub risk_release_frontier: f64,
    pub backlog_units: f64,
    pub anchor_price: f64,
    pub anchor_curve_target: f64,
    pub stale_release_elapsed_minutes: f64,
    pub stale_release_minutes: f64,
    pub next_advantage_target: f64,
    pub next_advantage_price: Option<f64>,
    pub next_release_units: f64,
    pub next_release_target: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackRiskAcquisitionDirection {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackReadStatus {
    WaitingMarketData,
    Active,
    Frozen,
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
pub enum TrackReadBindingIntent {
    IncreaseInventory,
    DecreaseInventory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackReadBindingPolicy {
    CurveMaker,
    CatchUp,
    ManualOverride,
    ReduceOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackReadBindingStatus {
    SubmitPending,
    Working,
    CancelPending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackRecoveryIssue {
    UnknownLiveOrder,
    DuplicateLiveOrders,
    AmbiguousLiveOrder,
    ExpectedExposureMismatch,
    BoundaryProgressOutOfRange,
}

impl From<RecoveryAnomaly> for TrackRecoveryIssue {
    fn from(value: RecoveryAnomaly) -> Self {
        match value {
            RecoveryAnomaly::UnknownLiveOrder => Self::UnknownLiveOrder,
            RecoveryAnomaly::DuplicateLiveOrders => Self::DuplicateLiveOrders,
            RecoveryAnomaly::AmbiguousLiveOrder => Self::AmbiguousLiveOrder,
            RecoveryAnomaly::ExpectedExposureMismatch => Self::ExpectedExposureMismatch,
            RecoveryAnomaly::BoundaryProgressOutOfRange => Self::BoundaryProgressOutOfRange,
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
pub struct TrackReadPnlStats {
    pub gross_realized_pnl_today: f64,
    pub gross_realized_pnl_cumulative: f64,
    pub trading_fee_today: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_today: f64,
    pub funding_fee_cumulative: f64,
}

impl TrackReadPnlStats {
    pub fn net_realized_pnl(&self) -> f64 {
        self.gross_realized_pnl_cumulative - self.trading_fee_cumulative
            + self.funding_fee_cumulative
    }
}

impl From<TrackPnlStats> for TrackReadPnlStats {
    fn from(value: TrackPnlStats) -> Self {
        Self {
            gross_realized_pnl_today: value.gross_realized_pnl_today,
            gross_realized_pnl_cumulative: value.gross_realized_pnl_cumulative,
            trading_fee_today: value.trading_fee_today,
            trading_fee_cumulative: value.trading_fee_cumulative,
            funding_fee_today: value.funding_fee_today,
            funding_fee_cumulative: value.funding_fee_cumulative,
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
        let track_config = definition.track_config();
        let recent_activity = project_recent_activity(recent_track_events, recent_effects);

        let bindings = project_bindings(&runtime);
        let inventory_gap = runtime
            .desired_exposure
            .as_ref()
            .map_or(0.0, |target| target.0 - runtime.current_exposure.0);

        Self {
            track_id: list_view.track_id.clone(),
            instrument: list_view.instrument.clone(),
            status: list_view.status,
            updated_at: list_view.updated_at,
            lower_price: track_config.lower_price,
            upper_price: track_config.upper_price,
            long_exposure_units: track_config.long_exposure_units,
            short_exposure_units: track_config.short_exposure_units,
            notional_per_unit: track_config.notional_per_unit,
            min_rebalance_units: track_config.min_rebalance_units,
            shape_family: track_config.shape_family,
            out_of_band_policy: track_config.out_of_band_policy,
            risk_acquisition_config: track_config.risk_acquisition,
            max_notional: definition.max_notional(),
            loss_limits: definition.loss_limits().clone(),
            strategy_price: list_view.strategy_price,
            strategy_price_status: list_view.strategy_price_status,
            mark_price: runtime.mark_price,
            best_bid: runtime.best_bid,
            best_ask: runtime.best_ask,
            current_exposure: list_view.current_exposure,
            position_qty: list_view.position_qty,
            desired_exposure: list_view.desired_exposure,
            risk_acquisition: list_view.risk_acquisition.clone(),
            pnl_stats: list_view.pnl_stats.clone(),
            unrealized_pnl: list_view.unrealized_pnl,
            inventory_gap,
            recovery_issue: list_view.recovery_issue,
            has_account_margin_guard: list_view.has_account_margin_guard,
            has_stale_market_data: list_view.has_stale_market_data,
            price_execution_block_reason: list_view.price_execution_block_reason,
            active_binding_count: list_view.active_binding_count,
            bindings,
            manual_target_override: runtime.manual_target_override.map(|value| value.0),
            recent_activity,
        }
    }
}

impl TrackListReadModel {
    pub(crate) fn from_parts(
        definition: &TrackDefinition,
        runtime: &TrackRuntimeView,
        updated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            track_id: definition.track_id().as_str().to_string(),
            instrument: definition.instrument().clone(),
            status: TrackReadStatus::from(runtime.status.clone()),
            updated_at,
            strategy_price: runtime.strategy_price,
            strategy_price_status: TrackStrategyPriceStatus::from(runtime.strategy_price_status),
            current_exposure: runtime.current_exposure.0,
            position_qty: runtime.position_qty,
            desired_exposure: runtime.desired_exposure.clone().map(|value| value.0),
            risk_acquisition: runtime
                .risk_acquisition
                .clone()
                .map(TrackRiskAcquisitionReadModel::from),
            pnl_stats: TrackReadPnlStats::from(runtime.pnl_stats.clone()),
            unrealized_pnl: runtime.unrealized_pnl,
            recovery_issue: runtime
                .executor
                .recovery_anomaly
                .clone()
                .map(TrackRecoveryIssue::from),
            has_account_margin_guard: runtime.has_account_margin_guard,
            has_stale_market_data: runtime.market_data_stale_since.is_some(),
            price_execution_block_reason: runtime
                .price_execution_block_reason
                .map(TrackPriceExecutionBlockReason::from),
            active_binding_count: runtime.executor.bindings.len() as u32,
        }
    }
}

impl From<&TrackReadModel> for TrackListReadModel {
    fn from(value: &TrackReadModel) -> Self {
        Self {
            track_id: value.track_id.clone(),
            instrument: value.instrument.clone(),
            status: value.status,
            updated_at: value.updated_at,
            strategy_price: value.strategy_price,
            strategy_price_status: value.strategy_price_status,
            current_exposure: value.current_exposure,
            position_qty: value.position_qty,
            desired_exposure: value.desired_exposure,
            risk_acquisition: value.risk_acquisition.clone(),
            pnl_stats: value.pnl_stats.clone(),
            unrealized_pnl: value.unrealized_pnl,
            recovery_issue: value.recovery_issue,
            has_account_margin_guard: value.has_account_margin_guard,
            has_stale_market_data: value.has_stale_market_data,
            price_execution_block_reason: value.price_execution_block_reason,
            active_binding_count: value.active_binding_count,
        }
    }
}

impl From<RiskAcquisitionRuntimeView> for TrackRiskAcquisitionReadModel {
    fn from(value: RiskAcquisitionRuntimeView) -> Self {
        Self {
            direction: TrackRiskAcquisitionDirection::from(value.direction),
            curve_target: value.curve_target.0,
            risk_release_frontier: value.risk_release_frontier.0,
            backlog_units: value.backlog_units,
            anchor_price: value.anchor_price,
            anchor_curve_target: value.anchor_curve_target.0,
            stale_release_elapsed_minutes: value.stale_release_elapsed_minutes,
            stale_release_minutes: value.stale_release_minutes,
            next_advantage_target: value.next_advantage_target.0,
            next_advantage_price: value.next_advantage_price,
            next_release_units: value.next_release_units,
            next_release_target: value.next_release_target.0,
        }
    }
}

impl From<RiskAcquisitionDirection> for TrackRiskAcquisitionDirection {
    fn from(value: RiskAcquisitionDirection) -> Self {
        match value {
            RiskAcquisitionDirection::Long => Self::Long,
            RiskAcquisitionDirection::Short => Self::Short,
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

fn project_bindings(runtime: &TrackRuntimeView) -> Vec<ReadModelBinding> {
    runtime
        .executor
        .bindings
        .iter()
        .enumerate()
        .map(|(index, binding)| ReadModelBinding {
            id: binding.id.clone(),
            policy: project_binding_policy(binding.policy),
            label: project_binding_label(index, binding.is_passive_execution),
            status: project_binding_status(binding.status),
            side: binding.side,
            price: binding.price,
            quantity: binding.quantity,
            intent: project_binding_intent(binding.increases_inventory),
        })
        .collect()
}

fn project_binding_intent(increases_inventory: bool) -> TrackReadBindingIntent {
    if increases_inventory {
        TrackReadBindingIntent::IncreaseInventory
    } else {
        TrackReadBindingIntent::DecreaseInventory
    }
}

fn project_binding_policy(policy: PolicyKind) -> TrackReadBindingPolicy {
    match policy {
        PolicyKind::CurveMaker => TrackReadBindingPolicy::CurveMaker,
        PolicyKind::CatchUp => TrackReadBindingPolicy::CatchUp,
        PolicyKind::ManualOverride => TrackReadBindingPolicy::ManualOverride,
        PolicyKind::ReduceOnly => TrackReadBindingPolicy::ReduceOnly,
    }
}

fn project_binding_status(status: BindingStatus) -> TrackReadBindingStatus {
    match status {
        BindingStatus::SubmitPending => TrackReadBindingStatus::SubmitPending,
        BindingStatus::Working => TrackReadBindingStatus::Working,
        BindingStatus::CancelPending => TrackReadBindingStatus::CancelPending,
        BindingStatus::Terminal => {
            unreachable!("terminal bindings should not be projected into read model")
        }
    }
}

fn project_binding_label(index: usize, is_passive_execution: bool) -> String {
    if is_passive_execution {
        format!("maker {}", index + 1)
    } else {
        format!("target {}", index + 1)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use poise_core::events::DomainEvent;
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::{BindingStatus, PolicyKind, SubmitRecoveryToken};
    use poise_engine::ports::OrderRequest;
    use poise_engine::runtime::{
        BindingView, ExecutorView, RiskAcquisitionDirection, RiskAcquisitionRuntimeView,
        StrategyPriceStatus, TrackRuntimeView, TrackStatus,
    };

    use super::{
        TrackActivityLevel, TrackPriceExecutionBlockReason, TrackReadBindingIntent, TrackReadModel,
        TrackReadStatus, TrackRiskAcquisitionDirection, TrackStrategyPriceStatus,
    };
    use crate::track_persistence::{EffectStatus, PersistedTrackEffect, StoredTrackEvent};
    use crate::track_read_source::TrackReadSource;

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
            risk_acquisition: Default::default(),
        }
    }

    fn test_track_definition() -> TrackDefinition {
        TrackDefinition::try_new(
            TrackId::new("btc-core"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            test_track_config(),
            Some(3000.0),
            LossLimits {
                daily_loss_limit: 100.0,
                total_loss_limit: 300.0,
            },
            Some(30),
        )
        .unwrap()
    }

    #[test]
    fn read_model_from_source_flattens_runtime_view() {
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime: TrackRuntimeView {
                status: TrackStatus::Active,
                current_exposure: Exposure(3.5),
                position_qty: 0.42,
                desired_exposure: Some(Exposure(4.0)),
                risk_acquisition: Default::default(),
                manual_target_override: None,
                executor: ExecutorView::default(),
                pnl_stats: Default::default(),
                unrealized_pnl: 0.0,
                has_account_margin_guard: false,
                price_execution_block_reason: None,
                strategy_price: None,
                strategy_price_status: StrategyPriceStatus::Stale,
                mark_price: None,
                best_bid: None,
                best_ask: None,
                last_tick_at: None,
                market_data_stale_since: None,
            },
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
                    recovery_token: SubmitRecoveryToken::empty(),
                },
                status: EffectStatus::Executing,
                attempt_count: 0,
                last_error: None,
                created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            }],
        });

        assert_eq!(read_model.track_id, "btc-core");
        assert_eq!(
            read_model.instrument,
            Instrument::new(Venue::Binance, "BTCUSDT")
        );
        assert_eq!(read_model.status, TrackReadStatus::Active);
        assert_eq!(read_model.position_qty, 0.42);
        assert_eq!(read_model.recovery_issue, None);
        assert_eq!(read_model.active_binding_count, 0);
        assert!(read_model.bindings.is_empty());
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
    fn read_model_exposes_strategy_price_status_and_best_bid_ask() {
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime: TrackRuntimeView {
                status: TrackStatus::Active,
                current_exposure: Exposure(1.0),
                position_qty: 1.0,
                desired_exposure: Some(Exposure(2.0)),
                risk_acquisition: Default::default(),
                manual_target_override: None,
                executor: ExecutorView::default(),
                pnl_stats: Default::default(),
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
                last_tick_at: None,
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
    fn read_model_uses_runtime_view_for_market_and_target_fields() {
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime: TrackRuntimeView {
                status: TrackStatus::Active,
                current_exposure: Exposure(1.0),
                position_qty: 1.0,
                desired_exposure: Some(Exposure(2.0)),
                risk_acquisition: Default::default(),
                manual_target_override: None,
                executor: ExecutorView::default(),
                pnl_stats: Default::default(),
                unrealized_pnl: 0.0,
                has_account_margin_guard: false,
                price_execution_block_reason: Some(
                    poise_engine::price_gate::PriceExecutionBlockReason::MissingExecutionQuote,
                ),
                strategy_price: Some(101.25),
                strategy_price_status: StrategyPriceStatus::Live,
                mark_price: Some(101.5),
                best_bid: Some(101.0),
                best_ask: Some(101.5),
                last_tick_at: None,
                market_data_stale_since: None,
            },
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

    #[test]
    fn read_model_preserves_risk_acquisition_observability() {
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime: TrackRuntimeView {
                status: TrackStatus::Active,
                current_exposure: Exposure(1.2),
                position_qty: 1.2,
                desired_exposure: Some(Exposure(1.2)),
                risk_acquisition: Some(RiskAcquisitionRuntimeView {
                    direction: RiskAcquisitionDirection::Long,
                    curve_target: Exposure(4.0),
                    risk_release_frontier: Exposure(1.2),
                    backlog_units: 2.8,
                    anchor_price: 95.0,
                    anchor_curve_target: Exposure(4.0),
                    stale_release_elapsed_minutes: 12.0,
                    stale_release_minutes: 30.0,
                    next_advantage_target: Exposure(6.0),
                    next_advantage_price: Some(92.5),
                    next_release_units: 1.0,
                    next_release_target: Exposure(2.2),
                }),
                manual_target_override: None,
                executor: ExecutorView::default(),
                pnl_stats: Default::default(),
                unrealized_pnl: 0.0,
                has_account_margin_guard: false,
                price_execution_block_reason: None,
                strategy_price: Some(95.0),
                strategy_price_status: StrategyPriceStatus::Live,
                mark_price: Some(95.0),
                best_bid: Some(94.9),
                best_ask: Some(95.1),
                last_tick_at: None,
                market_data_stale_since: None,
            },
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            recent_track_events: Vec::new(),
            recent_effects: Vec::new(),
        });

        let risk_acquisition = read_model
            .risk_acquisition
            .expect("risk acquisition should be projected");

        assert_eq!(
            risk_acquisition.direction,
            TrackRiskAcquisitionDirection::Long
        );
        assert!((risk_acquisition.curve_target - 4.0).abs() < 1e-9);
        assert!((risk_acquisition.risk_release_frontier - 1.2).abs() < 1e-9);
        assert!((risk_acquisition.backlog_units - 2.8).abs() < 1e-9);
        assert_eq!(risk_acquisition.next_advantage_price, Some(92.5));
    }

    #[test]
    fn read_model_derives_binding_intent_from_boundary_direction_not_reduce_only() {
        let runtime = TrackRuntimeView {
            status: TrackStatus::Active,
            current_exposure: Exposure(1.0),
            position_qty: 1.0,
            desired_exposure: Some(Exposure(0.0)),
            risk_acquisition: Default::default(),
            manual_target_override: None,
            executor: ExecutorView {
                bindings: vec![BindingView {
                    id: "curve-maker:positive-retrace".into(),
                    policy: PolicyKind::CurveMaker,
                    is_passive_execution: true,
                    status: BindingStatus::Working,
                    side: Side::Sell,
                    price: 101.0,
                    quantity: 1.0,
                    increases_inventory: false,
                }],
                recovery_anomaly: None,
            },
            pnl_stats: Default::default(),
            unrealized_pnl: 0.0,
            has_account_margin_guard: false,
            price_execution_block_reason: None,
            strategy_price: Some(101.0),
            strategy_price_status: StrategyPriceStatus::Live,
            mark_price: Some(101.0),
            best_bid: Some(100.9),
            best_ask: Some(101.1),
            last_tick_at: None,
            market_data_stale_since: None,
        };

        let bindings = super::project_bindings(&runtime);

        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].intent,
            TrackReadBindingIntent::DecreaseInventory
        );
    }
}
