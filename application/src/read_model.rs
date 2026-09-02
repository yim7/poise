use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use poise_core::events::{DomainEvent, ExecutionGateReason};
use poise_core::instrument::{Amount, InstrumentSpec, PositionBasis};
use poise_core::risk::LossLimits;
use poise_core::strategy::{BandProtectionPolicy, RiskAcquisitionConfig, ShapeFamily};
use poise_core::track::{Instrument, TrackDefinition};
use poise_core::types::Side;
use poise_engine::execution_plan::{DecisionReason, TrackEffect};
use poise_engine::executor::{BindingStatus, PolicyKind, RecoveryAnomaly};
use poise_engine::ledger::TrackPnlView;
use poise_engine::price_gate::PriceExecutionBlockReason;
use poise_engine::runtime::{
    AcquisitionResetReason, AcquisitionSessionRuntimeView, InventoryDecisionView,
    RiskAcquisitionDirection, RiskAcquisitionRuntimeView, StrategyPriceStatus, TrackRuntimeView,
    TrackStatus,
};
use serde::{Deserialize, Serialize};

use crate::account_read_model::AccountReadModel;
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
    pub instrument_spec: InstrumentSpec,
    pub status: TrackReadStatus,
    pub updated_at: DateTime<Utc>,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: TrackStrategyPriceStatus,
    pub current_exposure: f64,
    pub position_qty: f64,
    pub signed_position_notional: Option<Amount>,
    pub contract_base_exposure: Option<Amount>,
    pub decision: TrackInventoryDecisionReadModel,
    pub acquisition_session: Option<TrackAcquisitionSessionReadModel>,
    pub risk_acquisition: Option<TrackRiskAcquisitionReadModel>,
    pub pnl: TrackPnlView,
    pub recovery_issue: Option<TrackRecoveryIssue>,
    pub has_account_margin_guard: bool,
    pub has_stale_market_data: bool,
    pub price_execution_block_reason: Option<TrackPriceExecutionBlockReason>,
    pub active_binding_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInventoryDecisionReadModel {
    pub revision: Option<u64>,
    pub policy_target: Option<f64>,
    pub risk_approved_target: Option<f64>,
    pub acquisition_frontier: Option<f64>,
    pub execution_target: Option<f64>,
    pub reason: DecisionReason,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AccountAnalysisReadModel {
    pub tracks: Vec<AccountTrackAnalysisReadModel>,
    pub notional_exposures: Vec<AccountNotionalExposureReadModel>,
    pub base_exposures: Vec<AccountAssetExposureReadModel>,
    pub hedge_like: Vec<AccountHedgeLikeReadModel>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountTrackAnalysisReadModel {
    pub track_id: String,
    pub instrument: Instrument,
    pub settlement_asset: String,
    pub position_qty: f64,
    pub contract_count: Option<f64>,
    pub signed_notional: Option<f64>,
    pub abs_notional: Option<f64>,
    pub notional_asset: String,
    pub contract_base_asset: Option<String>,
    pub contract_base_exposure: Option<f64>,
    pub pnl_asset: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountNotionalExposureReadModel {
    pub asset: String,
    pub signed_notional: f64,
    pub abs_notional: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountAssetExposureReadModel {
    pub asset: String,
    pub quantity: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountHedgeLikeReadModel {
    pub asset: String,
    pub contract_base_exposure: f64,
    pub account_asset_quantity: Option<f64>,
    pub account_asset_quantity_source: Option<AccountAssetQuantitySource>,
    pub net_base_exposure: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountAssetQuantitySource {
    AccountSummaryAvailableByAsset,
}

pub fn build_account_analysis_read_model(
    tracks: &[TrackListReadModel],
) -> AccountAnalysisReadModel {
    build_account_analysis_read_model_with_account(None, tracks)
}

pub fn build_account_analysis_read_model_with_account(
    account: Option<&AccountReadModel>,
    tracks: &[TrackListReadModel],
) -> AccountAnalysisReadModel {
    let mut notional_exposures = BTreeMap::<String, (f64, f64)>::new();
    let mut base_exposures = BTreeMap::<String, f64>::new();
    let mut items = Vec::with_capacity(tracks.len());

    for track in tracks {
        let item = project_account_track_analysis(track);

        if let Some(signed_notional) = item.signed_notional {
            let exposure = notional_exposures
                .entry(item.notional_asset.clone())
                .or_default();
            exposure.0 += signed_notional;
            exposure.1 += signed_notional.abs();
        }
        if let (Some(asset), Some(quantity)) = (
            item.contract_base_asset.as_ref(),
            item.contract_base_exposure,
        ) {
            *base_exposures.entry(asset.clone()).or_default() += quantity;
        }

        items.push(item);
    }

    let notional_exposures = notional_exposures
        .into_iter()
        .map(
            |(asset, (signed_notional, abs_notional))| AccountNotionalExposureReadModel {
                asset,
                signed_notional,
                abs_notional,
            },
        )
        .collect();
    let base_exposures: Vec<_> = base_exposures
        .into_iter()
        .filter(|(_, quantity)| quantity.abs() > f64::EPSILON)
        .map(|(asset, quantity)| AccountAssetExposureReadModel { asset, quantity })
        .collect();
    let hedge_like = project_hedge_like(account, &base_exposures);

    AccountAnalysisReadModel {
        tracks: items,
        notional_exposures,
        base_exposures,
        hedge_like,
    }
}

fn project_hedge_like(
    account: Option<&AccountReadModel>,
    base_exposures: &[AccountAssetExposureReadModel],
) -> Vec<AccountHedgeLikeReadModel> {
    base_exposures
        .iter()
        .map(|exposure| {
            let account_asset_quantity = account.and_then(|account| {
                account
                    .available_by_asset
                    .get(exposure.asset.as_str())
                    .copied()
                    .filter(|value| value.is_finite())
            });

            AccountHedgeLikeReadModel {
                asset: exposure.asset.clone(),
                contract_base_exposure: exposure.quantity,
                account_asset_quantity,
                account_asset_quantity_source: account_asset_quantity
                    .map(|_| AccountAssetQuantitySource::AccountSummaryAvailableByAsset),
                net_base_exposure: account_asset_quantity
                    .map(|quantity| quantity + exposure.quantity),
            }
        })
        .collect()
}

fn project_account_track_analysis(source: &TrackListReadModel) -> AccountTrackAnalysisReadModel {
    let position_qty = finite_or_zero(source.position_qty);
    let signed_notional = source.signed_position_notional.as_ref().map(Amount::value);
    let contract_base_asset = source
        .contract_base_exposure
        .as_ref()
        .map(|amount| amount.asset().as_str().to_string());
    let contract_base_exposure = source.contract_base_exposure.as_ref().map(Amount::value);

    AccountTrackAnalysisReadModel {
        track_id: source.track_id.clone(),
        instrument: source.instrument.clone(),
        settlement_asset: source
            .instrument_spec
            .settlement_asset()
            .as_str()
            .to_string(),
        position_qty,
        contract_count: match source.instrument_spec.position_basis() {
            PositionBasis::BaseAsset => None,
            PositionBasis::Contracts { .. } => Some(position_qty),
        },
        signed_notional,
        abs_notional: signed_notional.map(f64::abs),
        notional_asset: source.instrument_spec.notional_asset().as_str().to_string(),
        contract_base_asset,
        contract_base_exposure,
        pnl_asset: source.pnl.asset.as_str().to_string(),
    }
}

fn finite_or_zero(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadModel {
    pub track_id: String,
    pub instrument: Instrument,
    pub instrument_spec: InstrumentSpec,
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
    pub signed_position_notional: Option<Amount>,
    pub contract_base_exposure: Option<Amount>,
    pub decision: TrackInventoryDecisionReadModel,
    pub acquisition_session: Option<TrackAcquisitionSessionReadModel>,
    pub risk_acquisition: Option<TrackRiskAcquisitionReadModel>,
    pub pnl: TrackPnlView,
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
    pub backlog_units: f64,
    pub release_anchor_price: f64,
    pub release_anchor_target: f64,
    pub stale_release_elapsed_minutes: f64,
    pub stale_release_minutes: f64,
    pub next_advantage_target: f64,
    pub next_advantage_price: Option<f64>,
    pub next_release_units: f64,
    pub next_release_target: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackAcquisitionSessionReadModel {
    pub acquisition_reset_reason: AcquisitionResetReason,
    pub acquisition_session_started_at: DateTime<Utc>,
    pub initial_frontier: Option<f64>,
    pub initial_release_units: Option<f64>,
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

        Self {
            track_id: list_view.track_id.clone(),
            instrument: list_view.instrument.clone(),
            instrument_spec: list_view.instrument_spec.clone(),
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
            signed_position_notional: list_view.signed_position_notional.clone(),
            contract_base_exposure: list_view.contract_base_exposure.clone(),
            decision: list_view.decision.clone(),
            acquisition_session: runtime
                .acquisition_session
                .clone()
                .map(TrackAcquisitionSessionReadModel::from),
            risk_acquisition: list_view.risk_acquisition.clone(),
            pnl: list_view.pnl.clone(),
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
            instrument_spec: runtime.instrument_spec.clone(),
            status: TrackReadStatus::from(runtime.status.clone()),
            updated_at,
            strategy_price: runtime.strategy_price,
            strategy_price_status: TrackStrategyPriceStatus::from(runtime.strategy_price_status),
            current_exposure: runtime.current_exposure.0,
            position_qty: runtime.position_qty,
            signed_position_notional: runtime.signed_position_notional.clone(),
            contract_base_exposure: runtime.contract_base_exposure.clone(),
            decision: TrackInventoryDecisionReadModel::from(&runtime.decision),
            acquisition_session: runtime
                .acquisition_session
                .clone()
                .map(TrackAcquisitionSessionReadModel::from),
            risk_acquisition: runtime
                .risk_acquisition
                .clone()
                .map(TrackRiskAcquisitionReadModel::from),
            pnl: runtime.pnl.clone(),
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
            instrument_spec: value.instrument_spec.clone(),
            status: value.status,
            updated_at: value.updated_at,
            strategy_price: value.strategy_price,
            strategy_price_status: value.strategy_price_status,
            current_exposure: value.current_exposure,
            position_qty: value.position_qty,
            signed_position_notional: value.signed_position_notional.clone(),
            contract_base_exposure: value.contract_base_exposure.clone(),
            decision: value.decision.clone(),
            acquisition_session: value.acquisition_session.clone(),
            risk_acquisition: value.risk_acquisition.clone(),
            pnl: value.pnl.clone(),
            recovery_issue: value.recovery_issue,
            has_account_margin_guard: value.has_account_margin_guard,
            has_stale_market_data: value.has_stale_market_data,
            price_execution_block_reason: value.price_execution_block_reason,
            active_binding_count: value.active_binding_count,
        }
    }
}

impl From<&InventoryDecisionView> for TrackInventoryDecisionReadModel {
    fn from(value: &InventoryDecisionView) -> Self {
        Self {
            revision: value.revision,
            policy_target: value.policy_target,
            risk_approved_target: value.risk_approved_target,
            acquisition_frontier: value.acquisition_frontier,
            execution_target: value.execution_target,
            reason: value.reason,
        }
    }
}

impl From<RiskAcquisitionRuntimeView> for TrackRiskAcquisitionReadModel {
    fn from(value: RiskAcquisitionRuntimeView) -> Self {
        Self {
            direction: TrackRiskAcquisitionDirection::from(value.direction),
            backlog_units: value.backlog_units,
            release_anchor_price: value.release_anchor_price,
            release_anchor_target: value.release_anchor_target.0,
            stale_release_elapsed_minutes: value.stale_release_elapsed_minutes,
            stale_release_minutes: value.stale_release_minutes,
            next_advantage_target: value.next_advantage_target.0,
            next_advantage_price: value.next_advantage_price,
            next_release_units: value.next_release_units,
            next_release_target: value.next_release_target.0,
        }
    }
}

impl From<AcquisitionSessionRuntimeView> for TrackAcquisitionSessionReadModel {
    fn from(value: AcquisitionSessionRuntimeView) -> Self {
        Self {
            acquisition_reset_reason: value.acquisition_reset_reason,
            acquisition_session_started_at: value.acquisition_session_started_at,
            initial_frontier: value.initial_frontier.map(|frontier| frontier.0),
            initial_release_units: value.initial_release_units,
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
            ExecutionGateReason::PnlIncomplete {
                expected_asset,
                actual_asset,
                unvalued_cashflow_count,
            } => format!(
                "execution gate: pnl incomplete expected {} got {} with {} unvalued cashflows",
                expected_asset,
                actual_asset.as_deref().unwrap_or("unknown"),
                unvalued_cashflow_count
            ),
        },
        DomainEvent::PnlAuditMissingRecords {
            missing_count,
            sample_source_keys,
        } => format!(
            "pnl audit: {missing_count} recent exchange fill records missing locally{}",
            format_source_key_sample(sample_source_keys)
        ),
    }
}

fn project_domain_event_level(event: &DomainEvent) -> TrackActivityLevel {
    match event {
        DomainEvent::ExecutionGateApplied { .. } | DomainEvent::PnlAuditMissingRecords { .. } => {
            TrackActivityLevel::Warn
        }
        _ => TrackActivityLevel::Info,
    }
}

fn format_source_key_sample(source_keys: &[String]) -> String {
    if source_keys.is_empty() {
        String::new()
    } else {
        format!(": {}", source_keys.join(", "))
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
        BindingStatus::SubmitPending | BindingStatus::SubmitAuthorized => {
            TrackReadBindingStatus::SubmitPending
        }
        BindingStatus::CancelAfterSubmit => TrackReadBindingStatus::CancelPending,
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
    use std::collections::BTreeMap;

    use crate::exchange::OrderRequest;
    use chrono::{TimeZone, Utc};
    use poise_core::events::DomainEvent;
    use poise_core::instrument::{Amount, Asset, InstrumentSpec};
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::execution_plan::{DecisionReason, TrackEffect};
    use poise_engine::executor::{BindingStatus, PolicyKind, SubmitRecoveryToken};
    use poise_engine::ledger::TrackPnlStats;
    use poise_engine::runtime::{
        AcquisitionResetReason, AcquisitionSessionRuntimeView, BindingView, ExecutorView,
        InventoryDecisionView, RiskAcquisitionDirection, RiskAcquisitionRuntimeView,
        StrategyPriceStatus, TrackRuntimeView, TrackStatus,
    };

    use super::{
        AccountAssetExposureReadModel, AccountAssetQuantitySource,
        AccountNotionalExposureReadModel, TrackActivityLevel, TrackInventoryDecisionReadModel,
        TrackListReadModel, TrackPriceExecutionBlockReason, TrackReadBindingIntent,
        TrackReadBindingStatus, TrackReadModel, TrackReadStatus, TrackRecoveryIssue,
        TrackRiskAcquisitionDirection, TrackRiskAcquisitionReadModel, TrackStrategyPriceStatus,
        project_binding_status,
    };
    use crate::track_persistence::{EffectStatus, PersistedTrackEffect, StoredTrackEvent};
    use crate::track_read_source::TrackReadSource;
    use crate::{AccountReadModel, AccountRiskSignal};

    #[test]
    fn read_model_folds_internal_submit_lifecycle_into_stable_public_statuses() {
        assert_eq!(
            project_binding_status(BindingStatus::SubmitAuthorized),
            TrackReadBindingStatus::SubmitPending
        );
        assert_eq!(
            project_binding_status(BindingStatus::CancelAfterSubmit),
            TrackReadBindingStatus::CancelPending
        );
    }

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

    fn test_instrument_spec() -> InstrumentSpec {
        crate::test_instrument::linear_spec(0.1, 0.001, 0.001, 5.0)
    }

    fn test_runtime_view(decision: InventoryDecisionView) -> TrackRuntimeView {
        let instrument_spec = test_instrument_spec();
        TrackRuntimeView {
            status: TrackStatus::Active,
            instrument_spec: instrument_spec.clone(),
            current_exposure: Exposure(1.0),
            position_qty: 1.0,
            signed_position_notional: Some(
                Amount::new(101.5, instrument_spec.notional_asset().clone()).unwrap(),
            ),
            contract_base_exposure: None,
            decision,
            acquisition_session: None,
            risk_acquisition: None,
            manual_target_override: None,
            executor: ExecutorView::default(),
            pnl_stats: Default::default(),
            pnl: TrackPnlStats::default().view(instrument_spec.settlement_asset(), 0.0),
            has_account_margin_guard: false,
            price_execution_block_reason: None,
            strategy_price: Some(101.25),
            strategy_price_status: StrategyPriceStatus::Live,
            mark_price: Some(101.5),
            best_bid: Some(101.0),
            best_ask: Some(101.5),
            last_tick_at: None,
            market_data_stale_since: None,
        }
    }

    fn test_track_list_read_model() -> TrackListReadModel {
        let instrument_spec = test_instrument_spec();
        TrackListReadModel {
            track_id: "btc-core".to_string(),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            instrument_spec: instrument_spec.clone(),
            status: TrackReadStatus::Active,
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            strategy_price: Some(100_000.0),
            strategy_price_status: TrackStrategyPriceStatus::Live,
            current_exposure: 0.0,
            position_qty: 0.42,
            signed_position_notional: Some(
                Amount::new(42_000.0, instrument_spec.notional_asset().clone()).unwrap(),
            ),
            contract_base_exposure: None,
            decision: TrackInventoryDecisionReadModel::from(&InventoryDecisionView::default()),
            acquisition_session: None,
            risk_acquisition: None,
            pnl: TrackPnlStats::default().view(instrument_spec.settlement_asset(), 0.0),
            recovery_issue: None,
            has_account_margin_guard: false,
            has_stale_market_data: false,
            price_execution_block_reason: None,
            active_binding_count: 0,
        }
    }

    fn set_inverse_position_values(track: &mut TrackListReadModel) {
        track.signed_position_notional =
            Some(Amount::new(-3_000.0, track.instrument_spec.notional_asset().clone()).unwrap());
        track.contract_base_exposure =
            Some(Amount::new(-0.03, track.instrument_spec.base_asset().clone()).unwrap());
        track.pnl = TrackPnlStats::default().view(track.instrument_spec.settlement_asset(), 0.0);
    }

    fn test_account_read_model() -> AccountReadModel {
        AccountReadModel {
            valuation_asset: Asset::try_new("USD").unwrap(),
            equity: 12_500.0,
            available: Some(9_000.0),
            available_by_asset: BTreeMap::from([(Asset::try_new("BTC").unwrap(), 0.20)]),
            unrealized_pnl: Some(-350.0),
            baseline_equity: 12_800.0,
            day_base_at: Utc.with_ymd_and_hms(2026, 4, 4, 0, 0, 1).unwrap(),
            day_change_pct: Some(-2.75),
            risk_signal: AccountRiskSignal::Attention,
            reason: Some("day_change -2.75%".to_string()),
            updated_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 23, 45).unwrap(),
        }
    }

    #[test]
    fn account_analysis_summarizes_inverse_contract_exposure() {
        let rules = crate::test_instrument::inverse_spec();

        let mut track = test_track_list_read_model();
        track.track_id = "btc-coin".to_string();
        track.instrument = Instrument::new(Venue::Okx, "BTC-USD-SWAP");
        track.instrument_spec = rules;
        track.position_qty = -30.0;
        set_inverse_position_values(&mut track);

        let analysis = super::build_account_analysis_read_model(&[track]);

        assert_eq!(
            analysis.notional_exposures,
            vec![AccountNotionalExposureReadModel {
                asset: "USD".to_string(),
                signed_notional: -3000.0,
                abs_notional: 3000.0,
            }]
        );
        assert_eq!(
            analysis.base_exposures,
            vec![AccountAssetExposureReadModel {
                asset: "BTC".to_string(),
                quantity: -0.03,
            }]
        );

        let item = &analysis.tracks[0];
        assert_eq!(item.track_id, "btc-coin");
        assert_eq!(item.settlement_asset, "BTC");
        assert_eq!(item.pnl_asset, "BTC");
        assert_eq!(item.contract_count, Some(-30.0));
        assert_eq!(item.signed_notional, Some(-3000.0));
        assert_eq!(item.abs_notional, Some(3000.0));
        assert_eq!(item.notional_asset, "USD");
        assert_eq!(item.contract_base_asset.as_deref(), Some("BTC"));
        assert_eq!(item.contract_base_exposure, Some(-0.03));
    }

    #[test]
    fn account_analysis_keeps_unknown_base_exposure_explicit() {
        let mut track = test_track_list_read_model();
        track.strategy_price = None;
        track.signed_position_notional = None;
        track.recovery_issue = Some(TrackRecoveryIssue::ExpectedExposureMismatch);

        let analysis = super::build_account_analysis_read_model(&[track]);

        assert_eq!(analysis.notional_exposures, Vec::new());
        assert_eq!(analysis.base_exposures, Vec::new());
        assert_eq!(analysis.tracks[0].settlement_asset, "USDT");
        assert_eq!(analysis.tracks[0].contract_count, None);
        assert_eq!(analysis.tracks[0].signed_notional, None);
        assert_eq!(analysis.tracks[0].abs_notional, None);
        assert_eq!(analysis.tracks[0].contract_base_asset, None);
        assert_eq!(analysis.tracks[0].contract_base_exposure, None);
    }

    #[test]
    fn account_analysis_keeps_notional_assets_separate() {
        let usdt_track = test_track_list_read_model();
        let mut usd_track = test_track_list_read_model();
        usd_track.track_id = "btc-coin".to_string();
        usd_track.instrument = Instrument::new(Venue::Okx, "BTC-USD-SWAP");
        usd_track.instrument_spec = crate::test_instrument::inverse_spec();
        usd_track.position_qty = -30.0;
        set_inverse_position_values(&mut usd_track);

        let analysis = super::build_account_analysis_read_model(&[usdt_track, usd_track]);

        assert_eq!(
            analysis.notional_exposures,
            vec![
                AccountNotionalExposureReadModel {
                    asset: "USD".to_string(),
                    signed_notional: -3000.0,
                    abs_notional: 3000.0,
                },
                AccountNotionalExposureReadModel {
                    asset: "USDT".to_string(),
                    signed_notional: 42_000.0,
                    abs_notional: 42_000.0,
                },
            ]
        );
    }

    #[test]
    fn account_analysis_estimates_hedge_like_net_exposure_from_account_asset_balance() {
        let rules = crate::test_instrument::inverse_spec();

        let mut track = test_track_list_read_model();
        track.instrument_spec = rules;
        track.position_qty = -30.0;
        set_inverse_position_values(&mut track);

        let account = test_account_read_model();
        let analysis =
            super::build_account_analysis_read_model_with_account(Some(&account), &[track]);

        assert_eq!(analysis.hedge_like.len(), 1);
        let hedge = &analysis.hedge_like[0];
        assert_eq!(hedge.asset, "BTC");
        assert_eq!(hedge.contract_base_exposure, -0.03);
        assert_eq!(hedge.account_asset_quantity, Some(0.20));
        assert_eq!(hedge.net_base_exposure, Some(0.17));
        assert_eq!(
            hedge.account_asset_quantity_source,
            Some(AccountAssetQuantitySource::AccountSummaryAvailableByAsset)
        );
    }

    #[test]
    fn account_analysis_marks_hedge_like_net_exposure_unknown_without_account_asset_balance() {
        let rules = crate::test_instrument::inverse_spec();

        let mut track = test_track_list_read_model();
        track.instrument_spec = rules;
        track.position_qty = -30.0;
        set_inverse_position_values(&mut track);

        let analysis = super::build_account_analysis_read_model_with_account(None, &[track]);

        assert_eq!(analysis.hedge_like.len(), 1);
        assert_eq!(analysis.hedge_like[0].asset, "BTC");
        assert_eq!(analysis.hedge_like[0].contract_base_exposure, -0.03);
        assert_eq!(analysis.hedge_like[0].account_asset_quantity, None);
        assert_eq!(analysis.hedge_like[0].net_base_exposure, None);
        assert_eq!(analysis.hedge_like[0].account_asset_quantity_source, None);
    }

    #[test]
    fn list_and_detail_project_one_atomic_inventory_decision() {
        let decision = InventoryDecisionView {
            revision: Some(7),
            policy_target: Some(8.0),
            risk_approved_target: Some(4.0),
            acquisition_frontier: Some(1.5),
            execution_target: Some(1.25),
            reason: DecisionReason::WaitingRiskRelease,
        };
        let runtime = test_runtime_view(decision);
        let updated_at = Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap();

        let list = TrackListReadModel::from_parts(&test_track_definition(), &runtime, updated_at);
        let detail = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime,
            updated_at,
            recent_track_events: Vec::new(),
            recent_effects: Vec::new(),
        });

        assert_eq!(list.decision, detail.decision);
        assert_eq!(detail.decision.revision, Some(7));
        assert_eq!(detail.decision.policy_target, Some(8.0));
        assert_eq!(detail.decision.risk_approved_target, Some(4.0));
        assert_eq!(detail.decision.acquisition_frontier, Some(1.5));
        assert_eq!(detail.decision.execution_target, Some(1.25));
        assert_eq!(detail.decision.reason, DecisionReason::WaitingRiskRelease);
    }

    #[test]
    fn acquisition_session_projects_without_active_backlog() {
        let started_at = Utc.with_ymd_and_hms(2026, 7, 1, 8, 0, 0).unwrap();
        let mut runtime = test_runtime_view(InventoryDecisionView::default());
        runtime.acquisition_session = Some(AcquisitionSessionRuntimeView {
            acquisition_reset_reason: AcquisitionResetReason::FreshSessionStart,
            acquisition_session_started_at: started_at,
            initial_frontier: Some(Exposure(2.0)),
            initial_release_units: Some(0.0),
        });
        assert!(runtime.risk_acquisition.is_none());
        let updated_at = Utc.with_ymd_and_hms(2026, 7, 1, 8, 1, 0).unwrap();

        let list = TrackListReadModel::from_parts(&test_track_definition(), &runtime, updated_at);
        let detail = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime,
            updated_at,
            recent_track_events: Vec::new(),
            recent_effects: Vec::new(),
        });

        assert_eq!(list.acquisition_session, detail.acquisition_session);
        let session = detail.acquisition_session.unwrap();
        assert_eq!(
            session.acquisition_reset_reason,
            AcquisitionResetReason::FreshSessionStart
        );
        assert_eq!(session.acquisition_session_started_at, started_at);
        assert_eq!(session.initial_frontier, Some(2.0));
        assert_eq!(session.initial_release_units, Some(0.0));
        assert!(detail.risk_acquisition.is_none());
    }

    #[test]
    fn unavailable_inventory_decision_preserves_reason_with_empty_targets() {
        let runtime = test_runtime_view(InventoryDecisionView {
            revision: None,
            policy_target: None,
            risk_approved_target: None,
            acquisition_frontier: None,
            execution_target: None,
            reason: DecisionReason::Paused,
        });

        let list = TrackListReadModel::from_parts(
            &test_track_definition(),
            &runtime,
            Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
        );

        assert_eq!(list.decision.revision, None);
        assert_eq!(list.decision.policy_target, None);
        assert_eq!(list.decision.risk_approved_target, None);
        assert_eq!(list.decision.acquisition_frontier, None);
        assert_eq!(list.decision.execution_target, None);
        assert_eq!(list.decision.reason, DecisionReason::Paused);
    }

    #[test]
    fn risk_acquisition_projection_contains_only_acquisition_diagnostics() {
        let mut runtime = test_runtime_view(InventoryDecisionView {
            revision: Some(7),
            policy_target: Some(8.0),
            risk_approved_target: Some(4.0),
            acquisition_frontier: Some(1.5),
            execution_target: Some(1.25),
            reason: DecisionReason::WaitingRiskRelease,
        });
        runtime.risk_acquisition = Some(RiskAcquisitionRuntimeView {
            direction: RiskAcquisitionDirection::Long,
            risk_approved_target: Exposure(4.0),
            acquisition_frontier: Exposure(1.5),
            backlog_units: 2.5,
            release_anchor_price: 95.0,
            release_anchor_target: Exposure(1.5),
            stale_release_elapsed_minutes: 12.0,
            stale_release_minutes: 30.0,
            next_advantage_target: Exposure(6.0),
            next_advantage_price: Some(92.5),
            next_release_units: 1.0,
            next_release_target: Exposure(2.5),
        });

        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime,
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            recent_track_events: Vec::new(),
            recent_effects: Vec::new(),
        });
        let TrackRiskAcquisitionReadModel {
            direction,
            backlog_units,
            release_anchor_price,
            release_anchor_target,
            stale_release_elapsed_minutes,
            stale_release_minutes,
            next_advantage_target,
            next_advantage_price,
            next_release_units,
            next_release_target,
        } = read_model
            .risk_acquisition
            .expect("risk acquisition diagnostics should be projected");

        assert_eq!(direction, TrackRiskAcquisitionDirection::Long);
        assert_eq!(backlog_units, 2.5);
        assert_eq!(release_anchor_price, 95.0);
        assert_eq!(release_anchor_target, 1.5);
        assert_eq!(stale_release_elapsed_minutes, 12.0);
        assert_eq!(stale_release_minutes, 30.0);
        assert_eq!(next_advantage_target, 6.0);
        assert_eq!(next_advantage_price, Some(92.5));
        assert_eq!(next_release_units, 1.0);
        assert_eq!(next_release_target, 2.5);
    }

    #[test]
    fn read_model_from_source_flattens_runtime_view() {
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime: TrackRuntimeView {
                current_exposure: Exposure(3.5),
                position_qty: 0.42,
                strategy_price: None,
                strategy_price_status: StrategyPriceStatus::Stale,
                mark_price: None,
                best_bid: None,
                best_ask: None,
                ..test_runtime_view(InventoryDecisionView {
                    revision: Some(1),
                    policy_target: Some(4.0),
                    risk_approved_target: Some(4.0),
                    acquisition_frontier: None,
                    execution_target: Some(4.0),
                    reason: DecisionReason::SubmitPlanned,
                })
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
                price_execution_block_reason: Some(
                    poise_engine::price_gate::PriceExecutionBlockReason::MissingExecutionQuote,
                ),
                strategy_price_status: StrategyPriceStatus::Stale,
                ..test_runtime_view(InventoryDecisionView {
                    revision: Some(2),
                    policy_target: Some(2.0),
                    risk_approved_target: Some(2.0),
                    acquisition_frontier: None,
                    execution_target: Some(2.0),
                    reason: DecisionReason::PriceGateBlocked,
                })
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
    fn read_model_uses_runtime_view_for_market_and_decision_fields() {
        let read_model = TrackReadModel::from_source(TrackReadSource {
            definition: test_track_definition(),
            runtime: TrackRuntimeView {
                price_execution_block_reason: Some(
                    poise_engine::price_gate::PriceExecutionBlockReason::MissingExecutionQuote,
                ),
                ..test_runtime_view(InventoryDecisionView {
                    revision: Some(3),
                    policy_target: Some(3.0),
                    risk_approved_target: Some(2.0),
                    acquisition_frontier: Some(1.5),
                    execution_target: Some(1.25),
                    reason: DecisionReason::WaitingRiskRelease,
                })
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
        assert_eq!(read_model.decision.revision, Some(3));
        assert_eq!(read_model.decision.policy_target, Some(3.0));
        assert_eq!(read_model.decision.risk_approved_target, Some(2.0));
        assert_eq!(read_model.decision.acquisition_frontier, Some(1.5));
        assert_eq!(read_model.decision.execution_target, Some(1.25));
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
                risk_acquisition: Some(RiskAcquisitionRuntimeView {
                    direction: RiskAcquisitionDirection::Long,
                    risk_approved_target: Exposure(4.0),
                    acquisition_frontier: Exposure(1.2),
                    backlog_units: 2.8,
                    release_anchor_price: 95.0,
                    release_anchor_target: Exposure(4.0),
                    stale_release_elapsed_minutes: 12.0,
                    stale_release_minutes: 30.0,
                    next_advantage_target: Exposure(6.0),
                    next_advantage_price: Some(92.5),
                    next_release_units: 1.0,
                    next_release_target: Exposure(2.2),
                }),
                strategy_price: Some(95.0),
                mark_price: Some(95.0),
                best_bid: Some(94.9),
                best_ask: Some(95.1),
                ..test_runtime_view(InventoryDecisionView {
                    revision: Some(4),
                    policy_target: Some(6.0),
                    risk_approved_target: Some(4.0),
                    acquisition_frontier: Some(1.2),
                    execution_target: Some(1.2),
                    reason: DecisionReason::WaitingRiskRelease,
                })
            },
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            recent_track_events: Vec::new(),
            recent_effects: Vec::new(),
        });

        let risk_acquisition = read_model
            .risk_acquisition
            .expect("risk acquisition should be projected");

        assert_eq!(read_model.decision.policy_target, Some(6.0));
        assert_eq!(read_model.decision.risk_approved_target, Some(4.0));
        assert_eq!(read_model.decision.acquisition_frontier, Some(1.2));
        assert_eq!(read_model.decision.execution_target, Some(1.2));
        assert_eq!(
            risk_acquisition.direction,
            TrackRiskAcquisitionDirection::Long
        );
        assert!((risk_acquisition.backlog_units - 2.8).abs() < 1e-9);
        assert_eq!(risk_acquisition.next_advantage_price, Some(92.5));
    }

    #[test]
    fn read_model_projects_binding_intent_from_runtime_view() {
        let runtime = TrackRuntimeView {
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
            strategy_price: Some(101.0),
            mark_price: Some(101.0),
            best_bid: Some(100.9),
            best_ask: Some(101.1),
            ..test_runtime_view(InventoryDecisionView {
                revision: Some(5),
                policy_target: Some(0.0),
                risk_approved_target: Some(0.0),
                acquisition_frontier: None,
                execution_target: Some(0.0),
                reason: DecisionReason::SubmitPlanned,
            })
        };

        let bindings = super::project_bindings(&runtime);

        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].intent,
            TrackReadBindingIntent::DecreaseInventory
        );
    }
}
