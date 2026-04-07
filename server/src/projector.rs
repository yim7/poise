use poise_engine::executor::{OrderRole, RecoveryAnomaly};
use poise_engine::ledger::{LedgerGapReason, LedgerGapRecord};
use poise_engine::runtime::TrackStatus as EngineTrackStatus;
use poise_protocol::{
    ExecutionBadgeView, ExecutionIntentView, ExecutionSlotOrderView, ExecutionSlotPhaseView,
    ExecutionSlotView, ExecutionStateView, ExecutionStatusView, ExposureSummaryView,
    InstrumentView, OutOfBandPolicy as ProtocolPolicy, ReplacementGateView,
    ShapeFamily as ProtocolShapeFamily, Side as ProtocolSide, TrackActivityItemView,
    TrackCommandType, TrackCommandView, TrackDetailView, TrackExecutionStatsView,
    TrackExecutionView, TrackIdentityView, TrackLedgerGapReasonView, TrackLedgerGapView, TrackLedgerView,
    TrackLifecycleView, TrackListItemView, TrackListLedgerView, TrackMarketView,
    TrackPositionView, TrackStatus as ProtocolTrackStatus, TrackStatusPanelView, TrackStrategyView,
};

use crate::event_presentation::project_activity_events;
use poise_application::TrackReadModel;

pub struct TrackProjector;

struct LedgerSummary {
    gross_realized_pnl: f64,
    net_realized_pnl: f64,
    total_pnl: f64,
    trading_fee_cumulative: f64,
    funding_fee_cumulative: f64,
    has_unresolved_gaps: bool,
}

impl TrackProjector {
    pub fn new() -> Self {
        Self
    }

    pub fn project_list_item(&self, source: &TrackReadModel) -> TrackListItemView {
        let ledger = project_ledger_summary(source);

        TrackListItemView {
            id: source.track_id.clone(),
            instrument: project_instrument(&source.venue, &source.symbol),
            lifecycle: TrackLifecycleView {
                status: project_track_status(&source.status),
                updated_at: source.updated_at.to_rfc3339(),
            },
            reference_price: source.reference_price,
            exposure: ExposureSummaryView {
                current: source.current_exposure,
                target: source.desired_exposure,
            },
            execution: ExecutionBadgeView {
                state: project_execution_state(source),
                execution_status: project_execution_status(source),
                active_slot_count: active_slot_count(source),
            },
            ledger: TrackListLedgerView {
                total_pnl: ledger.total_pnl,
                has_unresolved_gaps: ledger.has_unresolved_gaps,
            },
        }
    }

    pub fn project_detail(&self, source: &TrackReadModel) -> TrackDetailView {
        let ledger = project_ledger_summary(source);

        TrackDetailView {
            identity: TrackIdentityView {
                id: source.track_id.clone(),
                instrument: project_instrument(&source.venue, &source.symbol),
            },
            status: TrackStatusPanelView {
                lifecycle: TrackLifecycleView {
                    status: project_track_status(&source.status),
                    updated_at: source.updated_at.to_rfc3339(),
                },
                reference_price: source.reference_price,
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
            },
            market: TrackMarketView {
                mark_price: source.reference_price,
                index_price: source.reference_price,
            },
            position: TrackPositionView {
                current_exposure: source.current_exposure,
                desired_exposure: source.desired_exposure,
            },
            ledger: TrackLedgerView {
                gross_realized_pnl: ledger.gross_realized_pnl,
                net_realized_pnl: ledger.net_realized_pnl,
                unrealized_pnl: source.unrealized_pnl,
                total_pnl: ledger.total_pnl,
                trading_fee_cumulative: ledger.trading_fee_cumulative,
                funding_fee_cumulative: ledger.funding_fee_cumulative,
                unresolved_gaps: source
                    .ledger_state
                    .unresolved_gaps
                    .iter()
                    .map(project_ledger_gap)
                    .collect(),
            },
            execution_stats: TrackExecutionStatsView {
                max_inventory_gap_abs: source.max_inventory_gap_abs,
                max_gap_age_ms: source.max_gap_age_ms,
                stats_started_at: Some(source.stats_started_at.to_rfc3339()),
            },
            execution: TrackExecutionView {
                state: project_execution_state(source),
                execution_status: project_execution_status(source),
                attention_reasons: project_attention_reasons(source),
                inventory_gap: source.inventory_gap,
                gap_age_ms: source
                    .gap_started_at
                    .map(|started_at| (source.updated_at - started_at).num_milliseconds().max(0))
                    .unwrap_or(0),
                active_slot_count: active_slot_count(source),
                slots: project_execution_slots(source),
                replacement_gate: source
                    .replacement_gate_reason
                    .as_ref()
                    .map(project_replacement_gate_reason),
            },
            activity: self.project_activity(source),
            available_commands: project_available_commands(source),
        }
    }

    pub fn project_activity(&self, source: &TrackReadModel) -> Vec<TrackActivityItemView> {
        project_activity_events(source)
            .into_iter()
            .map(|item| TrackActivityItemView {
                ts: item.ts.to_rfc3339(),
                message: item.message,
                level: item.level,
            })
            .collect()
    }
}

fn project_ledger_summary(source: &TrackReadModel) -> LedgerSummary {
    let gross_realized_pnl = source.ledger_state.gross_realized_pnl_cumulative;
    let net_realized_pnl = source.ledger_state.net_realized_pnl();

    LedgerSummary {
        gross_realized_pnl,
        net_realized_pnl,
        total_pnl: net_realized_pnl + source.unrealized_pnl,
        trading_fee_cumulative: source.ledger_state.trading_fee_cumulative,
        funding_fee_cumulative: source.ledger_state.funding_fee_cumulative,
        has_unresolved_gaps: !source.ledger_state.unresolved_gaps.is_empty(),
    }
}

fn project_ledger_gap(gap: &LedgerGapRecord) -> TrackLedgerGapView {
    TrackLedgerGapView {
        gap_key: gap.gap_key.clone(),
        reason: project_ledger_gap_reason(gap.reason),
        observed_at: gap.observed_at.to_rfc3339(),
    }
}

fn project_ledger_gap_reason(reason: LedgerGapReason) -> TrackLedgerGapReasonView {
    match reason {
        LedgerGapReason::UnsupportedCommissionAsset => {
            TrackLedgerGapReasonView::UnsupportedCommissionAsset
        }
        LedgerGapReason::MissingCommissionAsset => TrackLedgerGapReasonView::MissingCommissionAsset,
        LedgerGapReason::MissingSymbol => TrackLedgerGapReasonView::MissingSymbol,
        LedgerGapReason::UnsupportedFundingAsset => TrackLedgerGapReasonView::UnsupportedFundingAsset,
    }
}

fn project_instrument(venue: &str, symbol: &str) -> InstrumentView {
    InstrumentView {
        venue: match venue {
            "binance" => "binance_futures".to_string(),
            other => other.to_string(),
        },
        symbol: symbol.to_string(),
    }
}

fn project_track_status(value: &EngineTrackStatus) -> ProtocolTrackStatus {
    match value {
        EngineTrackStatus::WaitingMarketData => ProtocolTrackStatus::WaitingMarketData,
        EngineTrackStatus::Active => ProtocolTrackStatus::Active,
        EngineTrackStatus::Frozen => ProtocolTrackStatus::Frozen,
        EngineTrackStatus::ReducingOnly => ProtocolTrackStatus::ReducingOnly,
        EngineTrackStatus::Holding => ProtocolTrackStatus::Holding,
        EngineTrackStatus::Terminated => ProtocolTrackStatus::Terminated,
        EngineTrackStatus::Paused => ProtocolTrackStatus::Paused,
    }
}

fn project_shape_family(value: poise_core::strategy::ShapeFamily) -> ProtocolShapeFamily {
    match value {
        poise_core::strategy::ShapeFamily::Linear => ProtocolShapeFamily::Linear,
        poise_core::strategy::ShapeFamily::Convex => ProtocolShapeFamily::Convex,
        poise_core::strategy::ShapeFamily::Concave => ProtocolShapeFamily::Concave,
    }
}

fn project_out_of_band_policy(value: poise_core::strategy::OutOfBandPolicy) -> ProtocolPolicy {
    match value {
        poise_core::strategy::OutOfBandPolicy::Freeze => ProtocolPolicy::Freeze,
        poise_core::strategy::OutOfBandPolicy::ReduceOnly => ProtocolPolicy::ReduceOnly,
        poise_core::strategy::OutOfBandPolicy::Terminate => ProtocolPolicy::Terminate,
        poise_core::strategy::OutOfBandPolicy::Hold => ProtocolPolicy::Hold,
    }
}

fn project_side(value: poise_core::types::Side) -> ProtocolSide {
    match value {
        poise_core::types::Side::Buy => ProtocolSide::Buy,
        poise_core::types::Side::Sell => ProtocolSide::Sell,
    }
}

fn project_replacement_gate_reason(
    reason: &poise_core::events::ReplacementGateReason,
) -> ReplacementGateView {
    match reason {
        poise_core::events::ReplacementGateReason::RoundedMatch => {
            ReplacementGateView::RoundedMatch
        }
        poise_core::events::ReplacementGateReason::ImprovementBelowThreshold {
            improvement_bps,
            threshold_bps,
        } => ReplacementGateView::ImprovementBelowThreshold {
            improvement_bps: *improvement_bps,
            threshold_bps: *threshold_bps,
        },
    }
}

fn project_execution_state(source: &TrackReadModel) -> ExecutionStateView {
    match source.status {
        EngineTrackStatus::Paused => ExecutionStateView::Paused,
        EngineTrackStatus::Terminated => ExecutionStateView::Closed,
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

fn project_attention_reasons(source: &TrackReadModel) -> Vec<String> {
    let mut reasons = Vec::new();

    if source.has_recovery_anomaly {
        reasons.push(
            source
                .recovery_anomaly
                .as_ref()
                .map(|anomaly| format!("recovery anomaly: {}", project_recovery_anomaly(anomaly)))
                .unwrap_or_else(|| "recovery anomaly".to_string()),
        );
    }

    if source.has_stale_market_data {
        reasons.push("market data stale".to_string());
    }

    if source.has_account_margin_guard {
        reasons.push("insufficient account margin".to_string());
    }

    reasons
}

fn project_recovery_anomaly(anomaly: &RecoveryAnomaly) -> &'static str {
    match anomaly {
        RecoveryAnomaly::UnknownLiveOrder => "unknown_live_order",
        RecoveryAnomaly::DuplicateLiveOrders => "duplicate_live_orders",
        RecoveryAnomaly::AmbiguousLiveOrder => "ambiguous_live_order",
    }
}

fn active_slot_count(source: &TrackReadModel) -> u32 {
    source.slots.len() as u32
}

fn project_execution_slots(source: &TrackReadModel) -> Vec<ExecutionSlotView> {
    source
        .slots
        .iter()
        .map(|slot| ExecutionSlotView {
            label: slot.label.clone(),
            phase: if slot.is_submit_pending {
                ExecutionSlotPhaseView::Opening
            } else {
                ExecutionSlotPhaseView::Working
            },
            intent: match slot.role {
                OrderRole::IncreaseInventory => ExecutionIntentView::IncreaseInventory,
                OrderRole::DecreaseInventory => ExecutionIntentView::DecreaseInventory,
            },
            order: Some(ExecutionSlotOrderView {
                side: project_side(slot.side),
                price: slot.price,
                quantity: slot.quantity,
            }),
        })
        .collect()
}

fn project_available_commands(source: &TrackReadModel) -> Vec<TrackCommandView> {
    let status = &source.status;
    vec![
        TrackCommandView {
            command: TrackCommandType::Pause,
            enabled: !matches!(
                status,
                EngineTrackStatus::Paused | EngineTrackStatus::Terminated
            ),
            disabled_reason: match status {
                EngineTrackStatus::Paused => Some("track is already paused".into()),
                EngineTrackStatus::Terminated => Some("terminated track cannot be paused".into()),
                _ => None,
            },
        },
        TrackCommandView {
            command: TrackCommandType::Resume,
            enabled: matches!(status, EngineTrackStatus::Paused)
                || source.manual_target_override.is_some(),
            disabled_reason: match status {
                EngineTrackStatus::Paused => None,
                _ if source.manual_target_override.is_some() => None,
                EngineTrackStatus::Terminated => Some("terminated track cannot be resumed".into()),
                _ => Some("track is not paused".into()),
            },
        },
        TrackCommandView {
            command: TrackCommandType::Terminate,
            enabled: !matches!(status, EngineTrackStatus::Terminated),
            disabled_reason: matches!(status, EngineTrackStatus::Terminated)
                .then_some("track is already terminated".into()),
        },
        TrackCommandView {
            command: TrackCommandType::Flatten,
            enabled: !matches!(status, EngineTrackStatus::Terminated),
            disabled_reason: matches!(status, EngineTrackStatus::Terminated)
                .then_some("terminated track cannot be flattened".into()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use poise_core::events::DomainEvent;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily};
    use poise_core::types::{Exposure, Side};
    use poise_engine::ledger::{LedgerGapReason, LedgerGapRecord, TrackLedgerState};
    use poise_engine::executor::{ExecutionMode, OrderRole};
    use poise_application::{EffectStatus, PersistedTrackEffect, StoredTrackEvent};
    use poise_engine::ports::OrderRequest;
    use poise_engine::runtime::TrackStatus;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use poise_protocol::{
        ActivityLevelView, ExecutionIntentView, ExecutionSlotPhaseView, ExecutionStateView,
        ExecutionStatusView, TrackCommandType, TrackLedgerGapReasonView,
    };

    use super::TrackProjector;
    use poise_application::{ReadModelSlot, TrackReadModel};

    #[test]
    fn projects_execution_badge_from_working_orders() {
        let source = source_with_submitting_effect();
        let item = TrackProjector::new().project_list_item(&source);
        let item_json = serde_json::to_value(&item).unwrap();

        assert_eq!(item.id, "btc-core");
        assert_eq!(item.execution.state, ExecutionStateView::Open);
        assert_eq!(item.execution.execution_status, ExecutionStatusView::Normal);
        assert_eq!(item.execution.active_slot_count, 1);
        assert_eq!(item.lifecycle.updated_at, "2026-03-26T10:01:30+00:00");
        assert!((item_json["ledger"]["total_pnl"].as_f64().unwrap() - 1229.0).abs() < 1e-9);
        assert_eq!(item_json["ledger"]["has_unresolved_gaps"].as_bool(), Some(true));

        let mut anomaly_source = source_with_submitting_effect();
        anomaly_source.has_recovery_anomaly = true;
        let anomaly_item = TrackProjector::new().project_list_item(&anomaly_source);
        assert_eq!(
            anomaly_item.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
    }

    #[test]
    fn projects_list_item_total_pnl_from_shared_ledger_summary() {
        let source = source_with_submitting_effect();
        let projector = TrackProjector::new();

        let item_json = serde_json::to_value(projector.project_list_item(&source)).unwrap();
        let detail_json = serde_json::to_value(projector.project_detail(&source)).unwrap();

        let item_total = item_json["ledger"]["total_pnl"].as_f64().unwrap();
        let detail_total = detail_json["ledger"]["total_pnl"].as_f64().unwrap();

        assert!((item_total - 1229.0).abs() < 1e-9);
        assert!((item_total - detail_total).abs() < 1e-9);
    }

    #[test]
    fn projects_list_item_lightweight_ledger_view() {
        let source = source_with_submitting_effect();
        let item_json = serde_json::to_value(TrackProjector::new().project_list_item(&source)).unwrap();

        assert!((item_json["ledger"]["total_pnl"].as_f64().unwrap() - 1229.0).abs() < 1e-9);
        assert_eq!(
            item_json["ledger"]["has_unresolved_gaps"].as_bool(),
            Some(true)
        );
        assert!(item_json.get("pnl").is_none());
    }

    #[test]
    fn projects_detail_ledger_from_unified_ledger_state() {
        let source = source_with_submitting_effect();
        let detail_json = serde_json::to_value(TrackProjector::new().project_detail(&source)).unwrap();

        assert_eq!(
            detail_json["ledger"]["gross_realized_pnl"].as_f64(),
            Some(980.1)
        );
        assert!(
            (detail_json["ledger"]["net_realized_pnl"].as_f64().unwrap() - 963.8).abs() < 1e-9
        );
        assert_eq!(
            detail_json["ledger"]["trading_fee_cumulative"].as_f64(),
            Some(12.3)
        );
        assert_eq!(
            detail_json["ledger"]["funding_fee_cumulative"].as_f64(),
            Some(-4.0)
        );
    }

    #[test]
    fn projects_all_unresolved_ledger_gaps() {
        let source = source_with_submitting_effect();
        let detail_json = serde_json::to_value(TrackProjector::new().project_detail(&source)).unwrap();

        assert_eq!(detail_json["ledger"]["unresolved_gaps"].as_array().unwrap().len(), 2);
        assert_eq!(
            detail_json["ledger"]["unresolved_gaps"][0]["gap_key"].as_str(),
            Some("binance:order_trade_update:btcusdt:12345:commission_asset")
        );
        assert_eq!(
            detail_json["ledger"]["unresolved_gaps"][1]["gap_key"].as_str(),
            Some("binance:funding_fee:btcusdt:2026-03-24T08:00:00+00:00:missing_symbol")
        );
    }

    #[test]
    fn projects_gap_reason_as_protocol_enum() {
        let detail = TrackProjector::new().project_detail(&source_with_submitting_effect());

        assert_eq!(
            detail.ledger.unresolved_gaps[0].reason,
            TrackLedgerGapReasonView::UnsupportedCommissionAsset
        );
        assert_eq!(
            detail.ledger.unresolved_gaps[1].reason,
            TrackLedgerGapReasonView::MissingSymbol
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
            detail_json["execution"]["slots"][0]["order"]
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
        source.status = TrackStatus::ReducingOnly;
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
        source.has_recovery_anomaly = true;
        source.recovery_anomaly =
            Some(poise_engine::executor::RecoveryAnomaly::DuplicateLiveOrders);
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
        source.has_recovery_anomaly = true;
        source.recovery_anomaly = None;

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
        assert_eq!(
            detail.execution.attention_reasons,
            vec!["recovery anomaly".to_string()]
        );
    }

    #[test]
    fn recovery_anomaly_projects_attention_reason() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.has_recovery_anomaly = true;
        source.recovery_anomaly = Some(poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder);

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.attention_reasons,
            vec!["recovery anomaly: unknown_live_order".to_string()]
        );
    }

    #[test]
    fn projects_execution_slots_from_slot_workset() {
        let detail = TrackProjector::new().project_detail(&source_with_submitting_effect());

        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert!((detail.execution.inventory_gap - 0.5).abs() < f64::EPSILON);
        assert_eq!(detail.execution.gap_age_ms, 90_000);
        assert_eq!(detail.execution.active_slot_count, 1);
        assert_eq!(
            detail.execution.active_slot_count,
            detail.execution.slots.len() as u32
        );
        assert_eq!(detail.execution.slots.len(), 1);
        assert_eq!(detail.execution.slots[0].label, "inventory");
        assert_eq!(
            detail.execution.slots[0].phase,
            ExecutionSlotPhaseView::Working
        );
        assert_eq!(
            detail.execution.slots[0].intent,
            ExecutionIntentView::IncreaseInventory
        );
        let order = detail.execution.slots[0].order.as_ref().unwrap();
        assert_eq!(order.side, poise_protocol::Side::Buy);
        assert!((order.price - 100.5).abs() < f64::EPSILON);
        assert!((order.quantity - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn projects_execution_observability_statistics() {
        let detail = TrackProjector::new().project_detail(&source_with_submitting_effect());

        assert!((detail.ledger.gross_realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((detail.ledger.net_realized_pnl - 963.8).abs() < 1e-9);
        assert!((detail.ledger.total_pnl - 1229.0).abs() < 1e-9);
        assert!((detail.ledger.unrealized_pnl - 265.2).abs() < f64::EPSILON);
        assert!((detail.execution_stats.max_inventory_gap_abs - 1.5).abs() < f64::EPSILON);
        assert_eq!(detail.execution_stats.max_gap_age_ms, 120_000);
        assert_eq!(
            detail.execution_stats.stats_started_at.as_deref(),
            Some("2026-03-26T09:45:00+00:00")
        );
    }

    #[test]
    fn project_detail_includes_replacement_gate_reason() {
        let mut source = source_with_submitting_effect();
        source.replacement_gate_reason = Some(
            poise_core::events::ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps: 9.0,
                threshold_bps: 13.0,
            },
        );

        let detail = TrackProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.replacement_gate,
            Some(
                poise_protocol::ReplacementGateView::ImprovementBelowThreshold {
                    improvement_bps: 9.0,
                    threshold_bps: 13.0,
                }
            )
        );
    }

    #[test]
    fn project_activity_distinguishes_superseded_submit_from_success() {
        let mut source = source_with_submitting_effect();
        source.recent_effects = vec![test_effect(EffectStatus::Superseded, None)];

        let activity = TrackProjector::new().project_activity(&source);

        assert_eq!(activity.len(), 1);
        assert_eq!(
            activity[0].message,
            "submit order superseded by newer track state"
        );
        assert_eq!(activity[0].level, ActivityLevelView::Info);
    }

    #[test]
    fn project_activity_renders_replacement_gate_event_message() {
        let mut source = source_with_submitting_effect();
        source.recent_track_events = vec![StoredTrackEvent {
            id: 1,
            track_id: TrackId::new("btc-core"),
            event: DomainEvent::ReplacementGateApplied {
                reason: poise_core::events::ReplacementGateReason::RoundedMatch,
            },
            created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
        }];

        let activity = TrackProjector::new().project_activity(&source);

        assert_eq!(activity.len(), 2);
        assert_eq!(
            activity[0].message,
            "replacement gate: candidate matches working order after rounding"
        );
        assert_eq!(activity[0].level, ActivityLevelView::Info);
    }

    #[test]
    fn project_activity_excludes_exposure_target_changed_events() {
        let source = source_with_failed_effect_and_recent_event();

        let activity = TrackProjector::new().project_activity(&source);

        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0].message, "submit order rejected");
    }

    fn source_with_submitting_effect() -> TrackReadModel {
        TrackReadModel {
            track_id: "btc-core".into(),
            venue: "binance".into(),
            symbol: "BTCUSDT".into(),
            status: TrackStatus::Active,
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
            reference_price: Some(101.25),
            current_exposure: 3.5,
            desired_exposure: Some(4.0),
            ledger_state: TrackLedgerState {
                realized_pnl_day: Some(chrono::NaiveDate::from_ymd_opt(2026, 3, 24).unwrap()),
                gross_realized_pnl_today: 980.1,
                gross_realized_pnl_cumulative: 980.1,
                trading_fee_cumulative: 12.3,
                funding_fee_cumulative: -4.0,
                unresolved_gaps: vec![
                    LedgerGapRecord {
                        gap_key:
                            "binance:order_trade_update:btcusdt:12345:commission_asset".into(),
                        reason: LedgerGapReason::UnsupportedCommissionAsset,
                        observed_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
                        source: "ORDER_TRADE_UPDATE".into(),
                    },
                    LedgerGapRecord {
                        gap_key:
                            "binance:funding_fee:btcusdt:2026-03-24T08:00:00+00:00:missing_symbol"
                                .into(),
                        reason: LedgerGapReason::MissingSymbol,
                        observed_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
                        source: "ACCOUNT_UPDATE:FUNDING_FEE".into(),
                    },
                ],
            },
            unrealized_pnl: 265.2,
            executor_mode: ExecutionMode::Passive,
            inventory_gap: 0.5,
            gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 26, 10, 0, 0).unwrap()),
            max_inventory_gap_abs: 1.5,
            max_gap_age_ms: 120_000,
            stats_started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
            recovery_anomaly: None,
            has_recovery_anomaly: false,
            has_account_margin_guard: false,
            has_stale_market_data: false,
            replacement_gate_reason: None,
            slots: vec![ReadModelSlot {
                label: "inventory".into(),
                is_submit_pending: false,
                side: Side::Buy,
                price: 100.5,
                quantity: 0.1,
                role: OrderRole::IncreaseInventory,
            }],
            manual_target_override: None,
            recent_track_events: Vec::new(),
            recent_effects: vec![test_effect(EffectStatus::Executing, None)],
        }
    }

    fn source_with_failed_effect_and_recent_event() -> TrackReadModel {
        TrackReadModel {
            recent_track_events: vec![StoredTrackEvent {
                id: 1,
                track_id: TrackId::new("btc-core"),
                event: DomainEvent::ExposureTargetChanged {
                    from: Exposure(3.5),
                    to: Exposure(4.0),
                },
                created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
            }],
            recent_effects: vec![test_effect(
                EffectStatus::Failed,
                Some("submit order rejected".into()),
            )],
            ..source_with_submitting_effect()
        }
    }

    fn test_effect(status: EffectStatus, last_error: Option<String>) -> PersistedTrackEffect {
        PersistedTrackEffect {
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
            },
            status,
            attempt_count: u32::from(matches!(status, EffectStatus::Failed)),
            last_error,
            created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
        }
    }
}
