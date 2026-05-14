use poise_protocol::{
    ActivityLevelView, BandFlattenTrigger as ProtocolTrigger,
    BandProtectionPolicy as ProtocolPolicy, BandRecoverPolicy as ProtocolRecoverPolicy,
    ExecutionBadgeView, ExecutionBindingIntentView, ExecutionBindingOrderView,
    ExecutionBindingPolicyView, ExecutionBindingStatusView, ExecutionBindingView,
    ExecutionStateView, ExecutionStatusView, ExposureSummaryView, InstrumentView,
    RiskAcquisitionConfigView, RiskAcquisitionDirectionView, RiskAcquisitionView,
    ShapeFamily as ProtocolShapeFamily, Side as ProtocolSide, StrategyPriceStatusView,
    TrackActivityItemView, TrackCommandType, TrackCommandView, TrackDetailView, TrackExecutionView,
    TrackIdentityView, TrackLifecycleView, TrackListItemView, TrackListPnlView,
    TrackLossLimitsView, TrackMarketView, TrackPnlView, TrackPositionView,
    TrackStatus as ProtocolTrackStatus, TrackStatusPanelView, TrackStrategyView,
};

use poise_core::track::Instrument;

use poise_application::{
    TrackActivityLevel, TrackListReadModel, TrackPriceExecutionBlockReason, TrackReadBindingIntent,
    TrackReadBindingPolicy, TrackReadBindingStatus, TrackReadModel, TrackReadStatus,
    TrackRecoveryIssue, TrackRiskAcquisitionDirection, TrackRiskAcquisitionReadModel,
    TrackStrategyPriceStatus,
};

pub struct TrackProjector;

struct PnlSummary {
    pnl_asset: String,
    gross_realized_pnl: f64,
    net_realized_pnl: f64,
    total_pnl: f64,
    trading_fee_cumulative: f64,
    funding_fee_cumulative: f64,
}

impl TrackProjector {
    pub fn new() -> Self {
        Self
    }

    pub fn project_list_item(&self, source: &TrackListReadModel) -> TrackListItemView {
        let pnl = project_list_pnl_summary(source);

        TrackListItemView {
            id: source.track_id.clone(),
            instrument: project_instrument(&source.instrument),
            lifecycle: TrackLifecycleView {
                status: project_track_status(&source.status),
                updated_at: source.updated_at.to_rfc3339(),
            },
            strategy_price: source.strategy_price,
            strategy_price_status: project_strategy_price_status(source.strategy_price_status),
            exposure: ExposureSummaryView {
                current: source.current_exposure,
                target: source.execution_target_exposure.or(source.desired_exposure),
            },
            execution: ExecutionBadgeView {
                state: project_list_execution_state(source),
                execution_status: project_list_execution_status(source),
                active_binding_count: source.active_binding_count,
            },
            pnl: TrackListPnlView {
                pnl_asset: pnl.pnl_asset,
                total_pnl: pnl.total_pnl,
            },
        }
    }

    pub fn project_detail(&self, source: &TrackReadModel) -> TrackDetailView {
        let pnl = project_detail_pnl_summary(source);

        TrackDetailView {
            identity: TrackIdentityView {
                id: source.track_id.clone(),
                instrument: project_instrument(&source.instrument),
            },
            status: TrackStatusPanelView {
                lifecycle: TrackLifecycleView {
                    status: project_track_status(&source.status),
                    updated_at: source.updated_at.to_rfc3339(),
                },
                strategy_price: source.strategy_price,
                strategy_price_status: project_strategy_price_status(source.strategy_price_status),
            },
            strategy: TrackStrategyView {
                lower_price: source.lower_price,
                upper_price: source.upper_price,
                long_exposure_units: source.long_exposure_units,
                short_exposure_units: source.short_exposure_units,
                notional_per_unit: source.notional_per_unit,
                min_rebalance_units: source.min_rebalance_units,
                shape_family: project_shape_family(source.shape_family),
                out_of_band_policy: project_out_of_band_policy(source.out_of_band_policy),
                risk_acquisition: project_risk_acquisition_config(source.risk_acquisition_config),
            },
            max_notional: source.max_notional,
            loss_limits: TrackLossLimitsView {
                daily_loss_limit: source.loss_limits.daily_loss_limit,
                total_loss_limit: source.loss_limits.total_loss_limit,
            },
            market: TrackMarketView {
                mark_price: source.mark_price,
                best_bid: source.best_bid,
                best_ask: source.best_ask,
            },
            position: TrackPositionView {
                current_exposure: source.current_exposure,
                desired_exposure: source.desired_exposure,
                quantity: source.position_qty,
                notional: project_position_notional(source),
                notional_asset: source.instrument.quote_asset(),
            },
            pnl: TrackPnlView {
                pnl_asset: pnl.pnl_asset,
                gross_realized_pnl: pnl.gross_realized_pnl,
                net_realized_pnl: pnl.net_realized_pnl,
                unrealized_pnl: source.unrealized_pnl,
                total_pnl: pnl.total_pnl,
                trading_fee_cumulative: pnl.trading_fee_cumulative,
                funding_fee_cumulative: pnl.funding_fee_cumulative,
            },
            execution: TrackExecutionView {
                state: project_execution_state(source),
                execution_status: project_execution_status(source),
                attention_reasons: project_attention_reasons(source),
                inventory_gap: source.inventory_gap,
                execution_target_exposure: source.execution_target_exposure,
                active_binding_count: source.active_binding_count,
                risk_acquisition: source
                    .risk_acquisition
                    .as_ref()
                    .map(project_risk_acquisition_runtime),
                bindings: project_execution_bindings(source),
            },
            activity: self.project_activity(source),
            available_commands: project_available_commands(source),
        }
    }

    pub fn project_activity(&self, source: &TrackReadModel) -> Vec<TrackActivityItemView> {
        source
            .recent_activity
            .iter()
            .map(|item| TrackActivityItemView {
                ts: item.ts.to_rfc3339(),
                message: item.message.clone(),
                level: project_activity_level(item.level),
            })
            .collect()
    }
}

fn project_list_pnl_summary(source: &TrackListReadModel) -> PnlSummary {
    let gross_realized_pnl = source.pnl_stats.gross_realized_pnl_cumulative;
    let net_realized_pnl = source.pnl_stats.net_realized_pnl();

    PnlSummary {
        pnl_asset: source.instrument.quote_asset(),
        gross_realized_pnl,
        net_realized_pnl,
        total_pnl: net_realized_pnl + source.unrealized_pnl,
        trading_fee_cumulative: source.pnl_stats.trading_fee_cumulative,
        funding_fee_cumulative: source.pnl_stats.funding_fee_cumulative,
    }
}

fn project_detail_pnl_summary(source: &TrackReadModel) -> PnlSummary {
    let gross_realized_pnl = source.pnl_stats.gross_realized_pnl_cumulative;
    let net_realized_pnl = source.pnl_stats.net_realized_pnl();

    PnlSummary {
        pnl_asset: source.instrument.quote_asset(),
        gross_realized_pnl,
        net_realized_pnl,
        total_pnl: net_realized_pnl + source.unrealized_pnl,
        trading_fee_cumulative: source.pnl_stats.trading_fee_cumulative,
        funding_fee_cumulative: source.pnl_stats.funding_fee_cumulative,
    }
}

fn project_activity_level(level: TrackActivityLevel) -> ActivityLevelView {
    match level {
        TrackActivityLevel::Info => ActivityLevelView::Info,
        TrackActivityLevel::Warn => ActivityLevelView::Warn,
        TrackActivityLevel::Error => ActivityLevelView::Error,
    }
}

fn project_instrument(instrument: &Instrument) -> InstrumentView {
    InstrumentView {
        venue: instrument.venue.as_str().to_string(),
        symbol: instrument.symbol.clone(),
    }
}

fn project_position_notional(source: &TrackReadModel) -> f64 {
    source
        .mark_price
        .or(source.strategy_price)
        .map_or(0.0, |price| source.position_qty * price)
}

fn project_track_status(value: &TrackReadStatus) -> ProtocolTrackStatus {
    match value {
        TrackReadStatus::WaitingMarketData => ProtocolTrackStatus::WaitingMarketData,
        TrackReadStatus::Active => ProtocolTrackStatus::Active,
        TrackReadStatus::Frozen => ProtocolTrackStatus::Frozen,
        TrackReadStatus::Flattening => ProtocolTrackStatus::Flattening,
        TrackReadStatus::ManualFlattening => ProtocolTrackStatus::ManualFlattening,
        TrackReadStatus::Terminated => ProtocolTrackStatus::Terminated,
        TrackReadStatus::Paused => ProtocolTrackStatus::Paused,
    }
}

fn project_risk_acquisition_runtime(source: &TrackRiskAcquisitionReadModel) -> RiskAcquisitionView {
    RiskAcquisitionView {
        direction: match source.direction {
            TrackRiskAcquisitionDirection::Long => RiskAcquisitionDirectionView::Long,
            TrackRiskAcquisitionDirection::Short => RiskAcquisitionDirectionView::Short,
        },
        curve_target: source.curve_target,
        risk_release_frontier: source.risk_release_frontier,
        backlog_units: source.backlog_units,
        anchor_price: source.anchor_price,
        anchor_curve_target: source.anchor_curve_target,
        stale_release_elapsed_minutes: source.stale_release_elapsed_minutes,
        stale_release_minutes: source.stale_release_minutes,
        next_advantage_target: source.next_advantage_target,
        next_advantage_price: source.next_advantage_price,
        next_release_units: source.next_release_units,
        next_release_target: source.next_release_target,
    }
}

fn project_strategy_price_status(value: TrackStrategyPriceStatus) -> StrategyPriceStatusView {
    match value {
        TrackStrategyPriceStatus::Live => StrategyPriceStatusView::Live,
        TrackStrategyPriceStatus::Stale => StrategyPriceStatusView::Stale,
    }
}

fn project_shape_family(value: poise_core::strategy::ShapeFamily) -> ProtocolShapeFamily {
    match value {
        poise_core::strategy::ShapeFamily::Linear => ProtocolShapeFamily::Linear,
        poise_core::strategy::ShapeFamily::Inertial => ProtocolShapeFamily::Inertial,
        poise_core::strategy::ShapeFamily::Responsive => ProtocolShapeFamily::Responsive,
    }
}

fn project_risk_acquisition_config(
    value: poise_core::strategy::RiskAcquisitionConfig,
) -> RiskAcquisitionConfigView {
    RiskAcquisitionConfigView {
        initial_ratio: value.initial_ratio,
        advantage_steps: value.advantage_steps,
        min_release_steps: value.min_release_steps,
        max_release_steps: value.max_release_steps,
        catchup_ratio: value.catchup_ratio,
        stale_release_minutes: value.stale_release_minutes,
    }
}

fn project_out_of_band_policy(value: poise_core::strategy::BandProtectionPolicy) -> ProtocolPolicy {
    match value {
        poise_core::strategy::BandProtectionPolicy::Freeze => ProtocolPolicy::Freeze,
        poise_core::strategy::BandProtectionPolicy::Flatten { trigger, recover } => {
            ProtocolPolicy::Flatten {
                trigger: project_band_flatten_trigger(trigger),
                recover: project_band_recover_policy(recover),
            }
        }
        poise_core::strategy::BandProtectionPolicy::Terminate => ProtocolPolicy::Terminate,
    }
}

fn project_band_flatten_trigger(
    value: poise_core::strategy::BandFlattenTrigger,
) -> ProtocolTrigger {
    match value {
        poise_core::strategy::BandFlattenTrigger::Immediate => ProtocolTrigger::Immediate,
        poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps } => {
            ProtocolTrigger::FlattenConfirm { bps }
        }
    }
}

fn project_band_recover_policy(
    value: poise_core::strategy::BandRecoverPolicy,
) -> ProtocolRecoverPolicy {
    match value {
        poise_core::strategy::BandRecoverPolicy::BackInBand => ProtocolRecoverPolicy::BackInBand,
        poise_core::strategy::BandRecoverPolicy::ReentryConfirm { bps } => {
            ProtocolRecoverPolicy::ReentryConfirm { bps }
        }
    }
}

fn project_side(value: poise_core::types::Side) -> ProtocolSide {
    match value {
        poise_core::types::Side::Buy => ProtocolSide::Buy,
        poise_core::types::Side::Sell => ProtocolSide::Sell,
    }
}

fn project_execution_state(source: &TrackReadModel) -> ExecutionStateView {
    match source.status {
        TrackReadStatus::Paused => ExecutionStateView::Paused,
        TrackReadStatus::Terminated => ExecutionStateView::Closed,
        _ => ExecutionStateView::Open,
    }
}

fn project_list_execution_state(source: &TrackListReadModel) -> ExecutionStateView {
    match source.status {
        TrackReadStatus::Paused => ExecutionStateView::Paused,
        TrackReadStatus::Terminated => ExecutionStateView::Closed,
        _ => ExecutionStateView::Open,
    }
}

fn project_execution_status(source: &TrackReadModel) -> ExecutionStatusView {
    if !project_attention_reasons(source).is_empty() {
        ExecutionStatusView::AttentionRequired
    } else {
        ExecutionStatusView::Normal
    }
}

fn project_list_execution_status(source: &TrackListReadModel) -> ExecutionStatusView {
    if !project_list_attention_reasons(source).is_empty() {
        ExecutionStatusView::AttentionRequired
    } else {
        ExecutionStatusView::Normal
    }
}

fn project_attention_reasons(source: &TrackReadModel) -> Vec<String> {
    let mut reasons = Vec::new();

    if let Some(issue) = source.recovery_issue.as_ref() {
        reasons.push(format!(
            "recovery anomaly: {}",
            project_recovery_issue(issue)
        ));
    }

    if source.has_stale_market_data {
        reasons.push("market data stale".to_string());
    }

    if source.has_account_margin_guard {
        reasons.push("insufficient account margin".to_string());
    }

    if let Some(reason) = source.price_execution_block_reason {
        reasons.push(project_price_execution_block_reason(reason).to_string());
    }

    reasons
}

fn project_list_attention_reasons(source: &TrackListReadModel) -> Vec<String> {
    let mut reasons = Vec::new();

    if let Some(issue) = source.recovery_issue.as_ref() {
        reasons.push(format!(
            "recovery anomaly: {}",
            project_recovery_issue(issue)
        ));
    }

    if source.has_stale_market_data {
        reasons.push("market data stale".to_string());
    }

    if source.has_account_margin_guard {
        reasons.push("insufficient account margin".to_string());
    }

    if let Some(reason) = source.price_execution_block_reason {
        reasons.push(project_price_execution_block_reason(reason).to_string());
    }

    reasons
}

fn project_price_execution_block_reason(reason: TrackPriceExecutionBlockReason) -> &'static str {
    match reason {
        TrackPriceExecutionBlockReason::MissingExecutionQuote => "missing execution quote",
        TrackPriceExecutionBlockReason::MarkBookDivergence => "mark/book divergence",
    }
}

fn project_recovery_issue(issue: &TrackRecoveryIssue) -> &'static str {
    match issue {
        TrackRecoveryIssue::UnknownLiveOrder => "unknown_live_order",
        TrackRecoveryIssue::DuplicateLiveOrders => "duplicate_live_orders",
        TrackRecoveryIssue::AmbiguousLiveOrder => "ambiguous_live_order",
        TrackRecoveryIssue::ExpectedExposureMismatch => "expected_exposure_mismatch",
        TrackRecoveryIssue::BoundaryProgressOutOfRange => "boundary_progress_out_of_range",
    }
}

fn project_execution_bindings(source: &TrackReadModel) -> Vec<ExecutionBindingView> {
    source
        .bindings
        .iter()
        .map(|binding| ExecutionBindingView {
            id: binding.id.clone(),
            policy: project_binding_policy(binding.policy),
            label: binding.label.clone(),
            status: project_binding_status(binding.status),
            intent: match binding.intent {
                TrackReadBindingIntent::IncreaseInventory => {
                    ExecutionBindingIntentView::IncreaseInventory
                }
                TrackReadBindingIntent::DecreaseInventory => {
                    ExecutionBindingIntentView::DecreaseInventory
                }
            },
            order: Some(ExecutionBindingOrderView {
                side: project_side(binding.side),
                price: binding.price,
                quantity: binding.quantity,
            }),
        })
        .collect()
}

fn project_binding_policy(policy: TrackReadBindingPolicy) -> ExecutionBindingPolicyView {
    match policy {
        TrackReadBindingPolicy::CurveMaker => ExecutionBindingPolicyView::CurveMaker,
        TrackReadBindingPolicy::CatchUp => ExecutionBindingPolicyView::CatchUp,
        TrackReadBindingPolicy::ManualOverride => ExecutionBindingPolicyView::ManualOverride,
        TrackReadBindingPolicy::ReduceOnly => ExecutionBindingPolicyView::ReduceOnly,
    }
}

fn project_binding_status(status: TrackReadBindingStatus) -> ExecutionBindingStatusView {
    match status {
        TrackReadBindingStatus::SubmitPending => ExecutionBindingStatusView::SubmitPending,
        TrackReadBindingStatus::Working => ExecutionBindingStatusView::Working,
        TrackReadBindingStatus::CancelPending => ExecutionBindingStatusView::CancelPending,
    }
}

fn project_available_commands(source: &TrackReadModel) -> Vec<TrackCommandView> {
    let status = &source.status;
    vec![
        TrackCommandView {
            command: TrackCommandType::Pause,
            enabled: !matches!(
                status,
                TrackReadStatus::Paused | TrackReadStatus::Terminated
            ),
            disabled_reason: match status {
                TrackReadStatus::Paused => Some("track is already paused".into()),
                TrackReadStatus::Terminated => Some("terminated track cannot be paused".into()),
                _ => None,
            },
        },
        TrackCommandView {
            command: TrackCommandType::Resume,
            enabled: matches!(
                status,
                TrackReadStatus::Paused | TrackReadStatus::ManualFlattening
            ),
            disabled_reason: match status {
                TrackReadStatus::Paused => None,
                TrackReadStatus::ManualFlattening => None,
                TrackReadStatus::Terminated => Some("terminated track cannot be resumed".into()),
                _ => Some("track is not paused".into()),
            },
        },
        TrackCommandView {
            command: TrackCommandType::Terminate,
            enabled: !matches!(status, TrackReadStatus::Terminated),
            disabled_reason: matches!(status, TrackReadStatus::Terminated)
                .then_some("track is already terminated".into()),
        },
        TrackCommandView {
            command: TrackCommandType::Flatten,
            enabled: !matches!(status, TrackReadStatus::Terminated),
            disabled_reason: matches!(status, TrackReadStatus::Terminated)
                .then_some("terminated track cannot be flattened".into()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use poise_application::{
        ReadModelBinding, TrackActivityEntry, TrackActivityLevel, TrackListReadModel,
        TrackPriceExecutionBlockReason, TrackReadBindingIntent, TrackReadBindingPolicy,
        TrackReadBindingStatus, TrackReadModel, TrackReadPnlStats, TrackReadStatus,
        TrackRecoveryIssue, TrackRiskAcquisitionDirection, TrackRiskAcquisitionReadModel,
        TrackStrategyPriceStatus,
    };
    use poise_core::strategy::{BandProtectionPolicy, BandRecoverPolicy, ShapeFamily};
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::Side;
    use poise_protocol::{
        ActivityLevelView, ExecutionBindingIntentView, ExecutionBindingPolicyView,
        ExecutionBindingStatusView, ExecutionStateView, ExecutionStatusView, TrackCommandType,
    };

    use super::TrackProjector;

    #[test]
    fn project_instrument_preserves_exchange_name() {
        let view = super::project_instrument(&Instrument::new(Venue::Binance, "BTCUSDT"));

        assert_eq!(view.venue, "binance");
        assert_eq!(view.symbol, "BTCUSDT");
    }

    #[test]
    fn projects_execution_badge_from_working_orders() {
        let source = list_source_with_submitting_effect();
        let item = TrackProjector::new().project_list_item(&source);
        let item_json = serde_json::to_value(&item).unwrap();

        assert_eq!(item.id, "btc-core");
        assert_eq!(item.execution.state, ExecutionStateView::Open);
        assert_eq!(item.execution.execution_status, ExecutionStatusView::Normal);
        assert_eq!(item.execution.active_binding_count, 1);
        assert_eq!(item.lifecycle.updated_at, "2026-03-26T10:01:30+00:00");
        assert!((item_json["pnl"]["total_pnl"].as_f64().unwrap() - 1229.0).abs() < 1e-9);

        let mut anomaly_source = list_source_with_submitting_effect();
        anomaly_source.recovery_issue = Some(TrackRecoveryIssue::UnknownLiveOrder);
        let anomaly_item = TrackProjector::new().project_list_item(&anomaly_source);
        assert_eq!(
            anomaly_item.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
    }

    #[test]
    fn projects_list_item_total_pnl_from_shared_pnl_summary() {
        let list_source = list_source_with_submitting_effect();
        let detail_source = source_with_submitting_effect();
        let projector = TrackProjector::new();

        let item_json = serde_json::to_value(projector.project_list_item(&list_source)).unwrap();
        let detail_json = serde_json::to_value(projector.project_detail(&detail_source)).unwrap();

        let item_total = item_json["pnl"]["total_pnl"].as_f64().unwrap();
        let detail_total = detail_json["pnl"]["total_pnl"].as_f64().unwrap();

        assert!((item_total - 1229.0).abs() < 1e-9);
        assert!((item_total - detail_total).abs() < 1e-9);
    }

    #[test]
    fn projects_list_item_exposure_target_from_execution_target() {
        let mut source = list_source_with_submitting_effect();
        source.desired_exposure = Some(4.0);
        source.execution_target_exposure = Some(3.75);

        let item = TrackProjector::new().project_list_item(&source);

        assert_eq!(item.exposure.current, 3.5);
        assert_eq!(item.exposure.target, Some(3.75));
    }

    #[test]
    fn projects_list_item_lightweight_pnl_view() {
        let source = list_source_with_submitting_effect();
        let item_json =
            serde_json::to_value(TrackProjector::new().project_list_item(&source)).unwrap();

        assert!((item_json["pnl"]["total_pnl"].as_f64().unwrap() - 1229.0).abs() < 1e-9);
        assert!(item_json["pnl"].get("gross_realized_pnl").is_none());
        assert!(item_json.get("ledger").is_none());
    }

    #[test]
    fn projector_preserves_existing_detail_and_list_shapes() {
        let list_source = list_source_with_submitting_effect();
        let detail_source = source_with_submitting_effect();
        let list_json =
            serde_json::to_value(TrackProjector::new().project_list_item(&list_source)).unwrap();
        let detail_json =
            serde_json::to_value(TrackProjector::new().project_detail(&detail_source)).unwrap();

        assert!(list_json.get("execution").is_some());
        assert!(list_json.get("exposure").is_some());
        assert!(detail_json.get("market").is_some());
        assert!(detail_json.get("position").is_some());
        assert_eq!(detail_json["market"]["mark_price"].as_f64(), Some(101.5));
        assert_eq!(
            detail_json["position"]["desired_exposure"].as_f64(),
            Some(4.0)
        );
    }

    #[test]
    fn projects_detail_pnl_from_track_pnl_stats() {
        let source = source_with_submitting_effect();
        let detail_json =
            serde_json::to_value(TrackProjector::new().project_detail(&source)).unwrap();

        assert_eq!(
            detail_json["pnl"]["gross_realized_pnl"].as_f64(),
            Some(980.1)
        );
        assert!((detail_json["pnl"]["net_realized_pnl"].as_f64().unwrap() - 963.8).abs() < 1e-9);
        assert_eq!(
            detail_json["pnl"]["trading_fee_cumulative"].as_f64(),
            Some(12.3)
        );
        assert_eq!(
            detail_json["pnl"]["funding_fee_cumulative"].as_f64(),
            Some(-4.0)
        );
    }

    #[test]
    fn project_detail_includes_available_commands_and_activity() {
        let source = source_with_failed_effect_and_recent_event();
        let detail = TrackProjector::new().project_detail(&source);
        let detail_json = serde_json::to_value(&detail).unwrap();

        assert_eq!(
            detail_json["strategy"]["long_exposure_units"].as_f64(),
            Some(8.0)
        );
        assert_eq!(
            detail_json["strategy"]["short_exposure_units"].as_f64(),
            Some(8.0)
        );
        assert_eq!(
            detail_json["strategy"]["notional_per_unit"].as_f64(),
            Some(375.0)
        );
        assert_eq!(
            detail_json["strategy"]["min_rebalance_units"].as_f64(),
            Some(0.5)
        );
        assert_eq!(
            detail_json["strategy"]["risk_acquisition"]["initial_ratio"].as_f64(),
            Some(0.5)
        );
        assert_eq!(
            detail_json["strategy"]["risk_acquisition"]["stale_release_minutes"].as_f64(),
            Some(60.0)
        );
        assert!(!detail.available_commands.is_empty());
        assert_eq!(
            detail.available_commands[0].command,
            TrackCommandType::Pause
        );
        assert_eq!(detail.available_commands.len(), 4);
        assert_eq!(
            detail.available_commands[2].command,
            TrackCommandType::Terminate
        );
        assert!(detail.available_commands[2].enabled);
        assert_eq!(detail.available_commands[2].disabled_reason, None);
        assert_eq!(
            detail.available_commands[3].command,
            TrackCommandType::Flatten
        );
        assert!(detail.available_commands[3].enabled);
        assert_eq!(detail.activity.len(), 1);
        assert_eq!(detail.activity[0].level, ActivityLevelView::Error);
        assert_eq!(detail.activity[0].message, "submit order rejected");
        assert!(
            detail
                .activity
                .iter()
                .all(|item| !item.message.contains("client-1"))
        );
        assert!(
            detail
                .activity
                .iter()
                .all(|item| !item.message.contains("desired exposure"))
        );
        assert!(
            detail_json["execution"]["bindings"][0]["order"]
                .get("client_order_id")
                .is_none()
        );
        assert!(detail.execution.attention_reasons.is_empty());
    }

    #[test]
    fn project_detail_enables_terminate_when_track_is_not_terminated() {
        let source = source_with_failed_effect_and_recent_event();

        let detail = TrackProjector::new().project_detail(&source);
        let terminate = detail
            .available_commands
            .iter()
            .find(|command| command.command == TrackCommandType::Terminate)
            .expect("terminate command should be present");

        assert!(terminate.enabled);
        assert_eq!(terminate.disabled_reason, None);
    }

    #[test]
    fn project_detail_enables_resume_when_manual_flatten_is_active() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.status = TrackReadStatus::ManualFlattening;
        source.manual_target_override = Some(0.0);

        let detail = TrackProjector::new().project_detail(&source);
        let resume = detail
            .available_commands
            .iter()
            .find(|command| command.command == TrackCommandType::Resume)
            .expect("resume command should be present");

        assert!(resume.enabled);
        assert_eq!(resume.disabled_reason, None);
    }

    #[test]
    fn resume_availability_depends_on_status_not_override() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.status = TrackReadStatus::Active;
        source.manual_target_override = Some(0.0);

        let detail = TrackProjector::new().project_detail(&source);
        let resume = detail
            .available_commands
            .iter()
            .find(|command| command.command == TrackCommandType::Resume)
            .expect("resume command should be present");

        assert!(!resume.enabled);
        assert_eq!(
            resume.disabled_reason,
            Some("track is not paused".to_string())
        );
    }

    #[test]
    fn project_out_of_band_policy_uses_flatten() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.out_of_band_policy = BandProtectionPolicy::Flatten {
            trigger: poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps: 500 },
            recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
        };

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(detail.strategy.out_of_band_policy.to_string(), "flatten");
    }

    #[test]
    fn projector_shows_flatten_trigger_and_recover_policy() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.status = TrackReadStatus::Active;
        source.out_of_band_policy = poise_core::strategy::BandProtectionPolicy::Flatten {
            trigger: poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps: 500 },
            recover: poise_core::strategy::BandRecoverPolicy::ReentryConfirm { bps: 500 },
        };

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            serde_json::to_value(detail.strategy.out_of_band_policy).unwrap(),
            serde_json::json!({
                "flatten": {
                    "trigger": { "flatten_confirm": { "bps": 500 } },
                    "recover": { "reentry_confirm": { "bps": 500 } }
                }
            })
        );
    }

    #[test]
    fn project_track_status_uses_manual_flattening() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.status = serde_json::from_str("\"manual_flattening\"").unwrap();

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.status.lifecycle.status.to_string(),
            "manual_flattening"
        );
    }

    #[test]
    fn projector_available_commands_follow_public_status_only() {
        let mut read_model = source_with_failed_effect_and_recent_event();
        read_model.status = TrackReadStatus::Paused;

        let detail = TrackProjector::new().project_detail(&read_model);

        assert!(
            detail
                .available_commands
                .iter()
                .any(|command| command.command == TrackCommandType::Resume && command.enabled)
        );
    }

    #[test]
    fn runtime_boundary_migration_removes_holding_from_public_status_projection() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.status = TrackReadStatus::Frozen;

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(detail.status.lifecycle.status.to_string(), "frozen");
    }

    #[test]
    fn stale_market_data_projects_attention_required() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.has_stale_market_data = true;

        let detail = TrackProjector::new().project_detail(&source);
        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
        assert_eq!(
            detail.execution.attention_reasons,
            vec!["market data stale".to_string()]
        );
    }

    #[test]
    fn projector_maps_price_gate_to_attention_required_reason() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.price_execution_block_reason =
            Some(TrackPriceExecutionBlockReason::MarkBookDivergence);

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
        assert!(
            detail
                .execution
                .attention_reasons
                .contains(&"mark/book divergence".to_string())
        );
    }

    #[test]
    fn projector_marks_strategy_price_status_stale_when_quote_is_missing() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.strategy_price = Some(101.25);
        source.strategy_price_status = TrackStrategyPriceStatus::Stale;
        source.best_bid = None;
        source.best_ask = None;
        source.price_execution_block_reason =
            Some(TrackPriceExecutionBlockReason::MissingExecutionQuote);

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.status.strategy_price_status,
            poise_protocol::StrategyPriceStatusView::Stale
        );
        assert_eq!(detail.market.best_bid, None);
        assert_eq!(detail.market.best_ask, None);
        assert!(
            detail
                .execution
                .attention_reasons
                .contains(&"missing execution quote".to_string())
        );
    }

    #[test]
    fn projector_uses_read_model_price_execution_block_reason_without_recomputing_gate() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.price_execution_block_reason =
            Some(TrackPriceExecutionBlockReason::MissingExecutionQuote);
        source.mark_price = Some(100.0);
        source.best_bid = Some(100.0);
        source.best_ask = Some(100.0);

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
        assert!(
            detail
                .execution
                .attention_reasons
                .contains(&"missing execution quote".to_string())
        );
    }

    #[test]
    fn account_margin_guard_projects_attention_reason_and_status() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.has_account_margin_guard = true;

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
        assert_eq!(
            detail.execution.attention_reasons,
            vec!["insufficient account margin".to_string()]
        );
    }

    #[test]
    fn multiple_attention_sources_preserve_reason_order_and_attention_status() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.recovery_issue = Some(TrackRecoveryIssue::DuplicateLiveOrders);
        source.has_stale_market_data = true;
        source.has_account_margin_guard = true;

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
        assert_eq!(
            detail.execution.attention_reasons,
            vec![
                "recovery anomaly: duplicate_live_orders".to_string(),
                "market data stale".to_string(),
                "insufficient account margin".to_string(),
            ]
        );
    }

    #[test]
    fn recovery_anomaly_without_specific_kind_still_projects_attention_reason() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.recovery_issue = None;

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert!(detail.execution.attention_reasons.is_empty());
    }

    #[test]
    fn recovery_anomaly_projects_attention_reason() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.recovery_issue = Some(TrackRecoveryIssue::UnknownLiveOrder);

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.attention_reasons,
            vec!["recovery anomaly: unknown_live_order".to_string()]
        );
    }

    #[test]
    fn recovery_attention_reason_is_derived_from_issue_without_duplicate_flag() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.recovery_issue = Some(TrackRecoveryIssue::DuplicateLiveOrders);

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.attention_reasons,
            vec!["recovery anomaly: duplicate_live_orders".to_string()]
        );
    }

    #[test]
    fn projects_execution_bindings_from_binding_workset() {
        let detail = TrackProjector::new().project_detail(&source_with_submitting_effect());

        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert!((detail.execution.inventory_gap - 0.5).abs() < f64::EPSILON);
        assert_eq!(detail.execution.execution_target_exposure, Some(4.0));
        assert_eq!(detail.execution.active_binding_count, 1);
        assert_eq!(
            detail.execution.active_binding_count,
            detail.execution.bindings.len() as u32
        );
        assert_eq!(detail.execution.bindings.len(), 1);
        assert_eq!(detail.execution.bindings[0].id, "binding-instance-1");
        assert_eq!(
            detail.execution.bindings[0].policy,
            ExecutionBindingPolicyView::CurveMaker
        );
        assert_eq!(detail.execution.bindings[0].label, "maker 1");
        assert_eq!(
            detail.execution.bindings[0].status,
            ExecutionBindingStatusView::Working
        );
        assert_eq!(
            detail.execution.bindings[0].intent,
            ExecutionBindingIntentView::IncreaseInventory
        );
        let order = detail.execution.bindings[0].order.as_ref().unwrap();
        assert_eq!(order.side, poise_protocol::Side::Buy);
        assert!((order.price - 100.5).abs() < f64::EPSILON);
        assert!((order.quantity - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn projects_risk_acquisition_observability() {
        let mut source = source_with_submitting_effect();
        source.risk_acquisition = Some(TrackRiskAcquisitionReadModel {
            direction: TrackRiskAcquisitionDirection::Long,
            curve_target: 6.0,
            risk_release_frontier: 2.375,
            backlog_units: 3.625,
            anchor_price: 100.0,
            anchor_curve_target: 4.0,
            stale_release_elapsed_minutes: 12.0,
            stale_release_minutes: 30.0,
            next_advantage_target: 6.0,
            next_advantage_price: Some(92.5),
            next_release_units: 0.875,
            next_release_target: 3.25,
        });

        let detail = TrackProjector::new().project_detail(&source);
        let risk_acquisition = detail
            .execution
            .risk_acquisition
            .expect("risk acquisition should be projected");

        assert_eq!(
            risk_acquisition.direction,
            poise_protocol::RiskAcquisitionDirectionView::Long
        );
        assert!((risk_acquisition.curve_target - 6.0).abs() < f64::EPSILON);
        assert!((risk_acquisition.backlog_units - 3.625).abs() < f64::EPSILON);
        assert_eq!(risk_acquisition.next_advantage_price, Some(92.5));
    }

    #[test]
    fn projects_execution_pnl_observability() {
        let detail = TrackProjector::new().project_detail(&source_with_submitting_effect());

        assert!((detail.pnl.gross_realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((detail.pnl.net_realized_pnl - 963.8).abs() < 1e-9);
        assert!((detail.pnl.total_pnl - 1229.0).abs() < 1e-9);
        assert!((detail.pnl.unrealized_pnl - 265.2).abs() < f64::EPSILON);
    }

    #[test]
    fn projects_position_quantity_from_observed_position_qty() {
        let mut source = source_with_submitting_effect();
        source.position_qty = 0.42;
        source.strategy_price = Some(101.25);
        let detail = TrackProjector::new().project_detail(&source);

        assert!((detail.position.quantity - 0.42).abs() < 1e-9);
        assert!((detail.position.notional - 42.63).abs() < 1e-9);
        assert_eq!(detail.position.notional_asset, "USDT");
    }

    #[test]
    fn projects_hyperliquid_perp_pnl_asset_as_usdc() {
        let mut source = source_with_submitting_effect();
        source.instrument = Instrument::new(Venue::Hyperliquid, "ETH");
        let detail = TrackProjector::new().project_detail(&source);
        let list = TrackProjector::new().project_list_item(&TrackListReadModel::from(&source));

        assert_eq!(detail.pnl.pnl_asset, "USDC");
        assert_eq!(list.pnl.pnl_asset, "USDC");
    }

    #[test]
    fn project_activity_preserves_application_activity_message_and_level() {
        let mut source = source_with_submitting_effect();
        source.recent_activity = vec![test_activity(
            Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            "submit order superseded by newer track state",
            TrackActivityLevel::Info,
        )];

        let activity = TrackProjector::new().project_activity(&source);

        assert_eq!(activity.len(), 1);
        assert_eq!(
            activity[0].message,
            "submit order superseded by newer track state"
        );
        assert_eq!(activity[0].level, ActivityLevelView::Info);
    }

    #[test]
    fn project_activity_preserves_application_activity_order() {
        let mut source = source_with_submitting_effect();
        source.recent_activity = vec![
            test_activity(
                Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
                "boundary ledger recovered active binding",
                TrackActivityLevel::Info,
            ),
            test_activity(
                Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                "submit order executing",
                TrackActivityLevel::Info,
            ),
        ];

        let activity = TrackProjector::new().project_activity(&source);

        assert_eq!(activity.len(), 2);
        assert_eq!(
            activity[0].message,
            "boundary ledger recovered active binding"
        );
        assert_eq!(activity[0].level, ActivityLevelView::Info);
    }

    #[test]
    fn project_activity_maps_application_error_level() {
        let mut source = source_with_submitting_effect();
        source.recent_activity = vec![test_activity(
            Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            "submit order rejected",
            TrackActivityLevel::Error,
        )];

        let activity = TrackProjector::new().project_activity(&source);

        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0].message, "submit order rejected");
        assert_eq!(activity[0].level, ActivityLevelView::Error);
    }

    fn source_with_submitting_effect() -> TrackReadModel {
        TrackReadModel {
            track_id: "btc-core".into(),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            status: TrackReadStatus::Active,
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition_config: Default::default(),
            max_notional: 3000.0,
            loss_limits: poise_core::risk::LossLimits {
                daily_loss_limit: 100.0,
                total_loss_limit: 300.0,
            },
            strategy_price: Some(101.25),
            strategy_price_status: TrackStrategyPriceStatus::Live,
            mark_price: Some(101.5),
            best_bid: Some(101.0),
            best_ask: Some(101.5),
            current_exposure: 3.5,
            position_qty: 13.125,
            desired_exposure: Some(4.0),
            execution_target_exposure: Some(4.0),
            risk_acquisition: Default::default(),
            pnl_stats: TrackReadPnlStats {
                gross_realized_pnl_today: 980.1,
                gross_realized_pnl_cumulative: 980.1,
                trading_fee_today: 0.0,
                trading_fee_cumulative: 12.3,
                funding_fee_today: 0.0,
                funding_fee_cumulative: -4.0,
            },
            unrealized_pnl: 265.2,
            inventory_gap: 0.5,
            recovery_issue: None,
            has_account_margin_guard: false,
            has_stale_market_data: false,
            price_execution_block_reason: None,
            active_binding_count: 1,
            bindings: vec![ReadModelBinding {
                id: "binding-instance-1".into(),
                policy: TrackReadBindingPolicy::CurveMaker,
                label: "maker 1".into(),
                status: TrackReadBindingStatus::Working,
                side: Side::Buy,
                price: 100.5,
                quantity: 0.1,
                intent: TrackReadBindingIntent::IncreaseInventory,
            }],
            manual_target_override: None,
            recent_activity: vec![test_activity(
                Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                "submit order executing",
                TrackActivityLevel::Info,
            )],
        }
    }

    fn source_with_failed_effect_and_recent_event() -> TrackReadModel {
        TrackReadModel {
            recent_activity: vec![test_activity(
                Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
                "submit order rejected",
                TrackActivityLevel::Error,
            )],
            ..source_with_submitting_effect()
        }
    }

    fn list_source_with_submitting_effect() -> TrackListReadModel {
        TrackListReadModel::from(&source_with_submitting_effect())
    }

    fn test_activity(
        ts: chrono::DateTime<Utc>,
        message: &str,
        level: TrackActivityLevel,
    ) -> TrackActivityEntry {
        TrackActivityEntry {
            ts,
            message: message.into(),
            level,
        }
    }
}
