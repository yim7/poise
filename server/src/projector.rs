use grid_core::events::DomainEvent;
use grid_engine::executor::OrderRole;
use grid_engine::ports::EffectStatus;
use grid_engine::runtime::GridStatus as EngineGridStatus;
use grid_engine::transition::GridEffect;
use grid_protocol::{
    ActivityLevelView, ExecutionBadgeView, ExecutionIntentView, ExecutionSlotOrderView,
    ExecutionSlotPhaseView, ExecutionSlotView, ExecutionStateView, ExecutionStatusView,
    ExposureSummaryView, GridActivityItemView, GridCommandType, GridCommandView, GridDetailView,
    GridExecutionView, GridIdentityView, GridLifecycleView, GridListItemView, GridMarketView,
    GridPositionView, GridStatisticsView, GridStatus as ProtocolGridStatus, GridStatusPanelView,
    GridStrategyView, InstrumentView, OutOfBandPolicy as ProtocolPolicy, ReplacementGateView,
    ShapeFamily as ProtocolShapeFamily, Side as ProtocolSide,
};

use crate::read_model::GridReadModel;

pub struct GridProjector;

impl GridProjector {
    pub fn new() -> Self {
        Self
    }

    pub fn project_list_item(&self, source: &GridReadModel) -> GridListItemView {
        GridListItemView {
            id: source.grid_id.clone(),
            instrument: project_instrument(&source.venue, &source.symbol),
            lifecycle: GridLifecycleView {
                status: project_grid_status(&source.status),
                updated_at: source.updated_at.to_rfc3339(),
            },
            reference_price: source.reference_price,
            exposure: ExposureSummaryView {
                current: source.current_exposure,
                target: source.target_exposure,
            },
            execution: ExecutionBadgeView {
                state: project_execution_state(source),
                execution_status: project_execution_status(source),
                active_slot_count: active_slot_count(source),
            },
        }
    }

    pub fn project_detail(&self, source: &GridReadModel) -> GridDetailView {
        GridDetailView {
            identity: GridIdentityView {
                id: source.grid_id.clone(),
                instrument: project_instrument(&source.venue, &source.symbol),
            },
            status: GridStatusPanelView {
                lifecycle: GridLifecycleView {
                    status: project_grid_status(&source.status),
                    updated_at: source.updated_at.to_rfc3339(),
                },
                reference_price: source.reference_price,
            },
            strategy: GridStrategyView {
                lower_price: source.lower_price,
                upper_price: source.upper_price,
                shape_family: project_shape_family(source.shape_family),
                out_of_band_policy: project_out_of_band_policy(source.out_of_band_policy),
            },
            market: GridMarketView {
                mark_price: source.reference_price,
                index_price: source.reference_price,
            },
            position: GridPositionView {
                current_exposure: source.current_exposure,
                target_exposure: source.target_exposure,
            },
            statistics: GridStatisticsView {
                total_pnl: source.realized_pnl_cumulative + source.unrealized_pnl,
                realized_pnl: source.realized_pnl_cumulative,
                max_inventory_gap_abs: source.max_inventory_gap_abs,
                max_gap_age_ms: source.max_gap_age_ms,
                stats_started_at: Some(source.stats_started_at.to_rfc3339()),
            },
            execution: GridExecutionView {
                state: project_execution_state(source),
                execution_status: project_execution_status(source),
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

    pub fn project_activity(&self, source: &GridReadModel) -> Vec<GridActivityItemView> {
        let mut activity = Vec::new();

        for event in &source.recent_domain_events {
            activity.push((
                event.created_at,
                GridActivityItemView {
                    ts: event.created_at.to_rfc3339(),
                    message: project_domain_event_message(&event.event),
                    level: project_domain_event_level(&event.event),
                },
            ));
        }

        for effect in &source.recent_effects {
            activity.push((
                effect.updated_at,
                GridActivityItemView {
                    ts: effect.updated_at.to_rfc3339(),
                    message: project_effect_message(effect),
                    level: project_effect_level(effect.status),
                },
            ));
        }

        activity.sort_by_key(|(ts, _)| *ts);
        activity.into_iter().map(|(_, item)| item).collect()
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

fn project_grid_status(value: &EngineGridStatus) -> ProtocolGridStatus {
    match value {
        EngineGridStatus::WaitingMarketData => ProtocolGridStatus::WaitingMarketData,
        EngineGridStatus::Active => ProtocolGridStatus::Active,
        EngineGridStatus::Frozen => ProtocolGridStatus::Frozen,
        EngineGridStatus::ReducingOnly => ProtocolGridStatus::ReducingOnly,
        EngineGridStatus::Holding => ProtocolGridStatus::Holding,
        EngineGridStatus::Terminated => ProtocolGridStatus::Terminated,
        EngineGridStatus::Paused => ProtocolGridStatus::Paused,
    }
}

fn project_shape_family(value: grid_core::strategy::ShapeFamily) -> ProtocolShapeFamily {
    match value {
        grid_core::strategy::ShapeFamily::Linear => ProtocolShapeFamily::Linear,
        grid_core::strategy::ShapeFamily::Convex => ProtocolShapeFamily::Convex,
        grid_core::strategy::ShapeFamily::Concave => ProtocolShapeFamily::Concave,
    }
}

fn project_out_of_band_policy(value: grid_core::strategy::OutOfBandPolicy) -> ProtocolPolicy {
    match value {
        grid_core::strategy::OutOfBandPolicy::Freeze => ProtocolPolicy::Freeze,
        grid_core::strategy::OutOfBandPolicy::ReduceOnly => ProtocolPolicy::ReduceOnly,
        grid_core::strategy::OutOfBandPolicy::Terminate => ProtocolPolicy::Terminate,
        grid_core::strategy::OutOfBandPolicy::Hold => ProtocolPolicy::Hold,
    }
}

fn project_side(value: grid_core::types::Side) -> ProtocolSide {
    match value {
        grid_core::types::Side::Buy => ProtocolSide::Buy,
        grid_core::types::Side::Sell => ProtocolSide::Sell,
    }
}

fn project_replacement_gate_reason(
    reason: &grid_core::events::ReplacementGateReason,
) -> ReplacementGateView {
    match reason {
        grid_core::events::ReplacementGateReason::RoundedMatch => ReplacementGateView::RoundedMatch,
        grid_core::events::ReplacementGateReason::ImprovementBelowThreshold {
            improvement_bps,
            threshold_bps,
        } => ReplacementGateView::ImprovementBelowThreshold {
            improvement_bps: *improvement_bps,
            threshold_bps: *threshold_bps,
        },
    }
}

fn project_execution_state(source: &GridReadModel) -> ExecutionStateView {
    match source.status {
        EngineGridStatus::Paused => ExecutionStateView::Paused,
        EngineGridStatus::Terminated => ExecutionStateView::Closed,
        _ => ExecutionStateView::Open,
    }
}

fn project_execution_status(source: &GridReadModel) -> ExecutionStatusView {
    if source.has_recovery_anomaly || source.has_stale_market_data {
        ExecutionStatusView::AttentionRequired
    } else {
        ExecutionStatusView::Normal
    }
}

fn active_slot_count(source: &GridReadModel) -> u32 {
    source.slots.len() as u32
}

fn project_execution_slots(source: &GridReadModel) -> Vec<ExecutionSlotView> {
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

fn project_available_commands(source: &GridReadModel) -> Vec<GridCommandView> {
    let status = &source.status;
    vec![
        GridCommandView {
            command: GridCommandType::Pause,
            enabled: !matches!(
                status,
                EngineGridStatus::Paused | EngineGridStatus::Terminated
            ),
            disabled_reason: match status {
                EngineGridStatus::Paused => Some("grid is already paused".into()),
                EngineGridStatus::Terminated => Some("terminated grid cannot be paused".into()),
                _ => None,
            },
        },
        GridCommandView {
            command: GridCommandType::Resume,
            enabled: matches!(status, EngineGridStatus::Paused)
                || source.manual_target_override.is_some(),
            disabled_reason: match status {
                EngineGridStatus::Paused => None,
                _ if source.manual_target_override.is_some() => None,
                EngineGridStatus::Terminated => Some("terminated grid cannot be resumed".into()),
                _ => Some("grid is not paused".into()),
            },
        },
        GridCommandView {
            command: GridCommandType::Terminate,
            enabled: false,
            disabled_reason: Some(
                match status {
                    EngineGridStatus::Terminated => "grid is already terminated",
                    _ => "terminate command is not implemented",
                }
                .into(),
            ),
        },
        GridCommandView {
            command: GridCommandType::Flatten,
            enabled: !matches!(status, EngineGridStatus::Terminated),
            disabled_reason: matches!(status, EngineGridStatus::Terminated)
                .then_some("terminated grid cannot be flattened".into()),
        },
    ]
}

fn project_domain_event_message(event: &DomainEvent) -> String {
    match event {
        DomainEvent::ExposureTargetChanged { from, to } => {
            format!("target exposure {:.4} -> {:.4}", from.0, to.0)
        }
        DomainEvent::BandBreached { boundary, price } => {
            format!("band breached {:?} at {:.4}", boundary, price)
        }
        DomainEvent::BandReentered { price } => format!("band reentered at {:.4}", price),
        DomainEvent::PolicyTriggered { policy } => format!("policy triggered: {:?}", policy),
        DomainEvent::RiskCapApplied { intended, capped } => {
            format!("risk cap {:.4} -> {:.4}", intended.0, capped.0)
        }
        DomainEvent::RiskDenied { reason } => format!("risk denied: {reason}"),
        DomainEvent::ReplacementGateApplied { reason } => match reason {
            grid_core::events::ReplacementGateReason::RoundedMatch => {
                "replacement gate: candidate matches working order after rounding".into()
            }
            grid_core::events::ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps,
                threshold_bps,
            } => format!(
                "replacement gate: improvement {:.1} bps < threshold {:.1} bps",
                improvement_bps, threshold_bps
            ),
        },
    }
}

fn project_domain_event_level(event: &DomainEvent) -> ActivityLevelView {
    match event {
        DomainEvent::RiskDenied { .. } => ActivityLevelView::Warn,
        _ => ActivityLevelView::Info,
    }
}

fn project_effect_message(effect: &grid_engine::ports::PersistedGridEffect) -> String {
    match &effect.effect {
        GridEffect::SubmitOrder { .. } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| "submit order failed".into()),
            EffectStatus::Succeeded => "submit order succeeded".into(),
            EffectStatus::Superseded => "submit order superseded by newer grid state".into(),
            EffectStatus::Executing => "submit order executing".into(),
            EffectStatus::Pending => "submit order pending".into(),
        },
        GridEffect::CancelOrder { order_id, .. } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| format!("cancel {order_id} failed")),
            EffectStatus::Succeeded => format!("cancel {order_id} succeeded"),
            EffectStatus::Superseded => format!("cancel {order_id} superseded"),
            EffectStatus::Executing => format!("cancel {order_id} executing"),
            EffectStatus::Pending => format!("cancel {order_id} pending"),
        },
        GridEffect::CancelAll { instrument } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| format!("cancel all {} failed", instrument.symbol)),
            EffectStatus::Succeeded => format!("cancel all {} succeeded", instrument.symbol),
            EffectStatus::Superseded => format!("cancel all {} superseded", instrument.symbol),
            EffectStatus::Executing => format!("cancel all {} executing", instrument.symbol),
            EffectStatus::Pending => format!("cancel all {} pending", instrument.symbol),
        },
        GridEffect::NoOp => "no-op".into(),
    }
}

fn project_effect_level(status: EffectStatus) -> ActivityLevelView {
    match status {
        EffectStatus::Failed => ActivityLevelView::Error,
        _ => ActivityLevelView::Info,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use grid_core::events::DomainEvent;
    use grid_core::strategy::{OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{Exposure, Side};
    use grid_engine::executor::{ExecutionMode, OrderRole};
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::ports::{
        EffectStatus, OrderRequest, PersistedGridEffect, StoredDomainEvent,
    };
    use grid_engine::runtime::GridStatus;
    use grid_engine::transition::GridEffect;
    use grid_protocol::{
        ActivityLevelView, ExecutionIntentView, ExecutionSlotPhaseView, ExecutionStateView,
        ExecutionStatusView, GridCommandType,
    };

    use super::GridProjector;
    use crate::read_model::{GridReadModel, ReadModelSlot};

    #[test]
    fn projects_execution_badge_from_working_orders() {
        let source = source_with_submitting_effect();
        let item = GridProjector::new().project_list_item(&source);

        assert_eq!(item.id, "btc-core");
        assert_eq!(item.execution.state, ExecutionStateView::Open);
        assert_eq!(item.execution.execution_status, ExecutionStatusView::Normal);
        assert_eq!(item.execution.active_slot_count, 1);
        assert_eq!(item.lifecycle.updated_at, "2026-03-26T10:01:30+00:00");

        let mut anomaly_source = source_with_submitting_effect();
        anomaly_source.has_recovery_anomaly = true;
        let anomaly_item = GridProjector::new().project_list_item(&anomaly_source);
        assert_eq!(
            anomaly_item.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
    }

    #[test]
    fn project_detail_includes_available_commands_and_activity() {
        let source = source_with_failed_effect_and_recent_event();
        let detail = GridProjector::new().project_detail(&source);
        let detail_json = serde_json::to_value(&detail).unwrap();

        assert!(!detail.available_commands.is_empty());
        assert_eq!(detail.available_commands[0].command, GridCommandType::Pause);
        assert_eq!(detail.available_commands.len(), 4);
        assert_eq!(
            detail.available_commands[2].command,
            GridCommandType::Terminate
        );
        assert!(!detail.available_commands[2].enabled);
        assert_eq!(
            detail.available_commands[3].command,
            GridCommandType::Flatten
        );
        assert!(detail.available_commands[3].enabled);
        assert_eq!(detail.activity.len(), 2);
        assert_eq!(detail.activity[0].level, ActivityLevelView::Info);
        assert_eq!(detail.activity[1].level, ActivityLevelView::Error);
        assert!(
            detail
                .activity
                .iter()
                .all(|item| !item.message.contains("client-1"))
        );
        assert!(
            detail_json["execution"]["slots"][0]["order"]
                .get("client_order_id")
                .is_none()
        );
    }

    #[test]
    fn project_detail_enables_resume_when_manual_flatten_is_active() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.status = GridStatus::ReducingOnly;
        source.manual_target_override = Some(0.0);

        let detail = GridProjector::new().project_detail(&source);
        let resume = detail
            .available_commands
            .iter()
            .find(|command| command.command == GridCommandType::Resume)
            .expect("resume command should be present");

        assert!(resume.enabled);
        assert_eq!(resume.disabled_reason, None);
    }

    #[test]
    fn stale_market_data_projects_attention_required() {
        let mut source = source_with_failed_effect_and_recent_event();
        source.has_stale_market_data = true;

        let detail = GridProjector::new().project_detail(&source);
        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::AttentionRequired
        );
    }

    #[test]
    fn projects_execution_slots_from_slot_workset() {
        let detail = GridProjector::new().project_detail(&source_with_submitting_effect());

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
        assert_eq!(order.side, grid_protocol::Side::Buy);
        assert!((order.price - 100.5).abs() < f64::EPSILON);
        assert!((order.quantity - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn projects_execution_observability_statistics() {
        let detail = GridProjector::new().project_detail(&source_with_submitting_effect());

        assert!((detail.statistics.realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((detail.statistics.total_pnl - 1245.3).abs() < f64::EPSILON);
        assert!((detail.statistics.max_inventory_gap_abs - 1.5).abs() < f64::EPSILON);
        assert_eq!(detail.statistics.max_gap_age_ms, 120_000);
        assert_eq!(
            detail.statistics.stats_started_at.as_deref(),
            Some("2026-03-26T09:45:00+00:00")
        );
    }

    #[test]
    fn project_detail_includes_replacement_gate_reason() {
        let mut source = source_with_submitting_effect();
        source.replacement_gate_reason = Some(
            grid_core::events::ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps: 9.0,
                threshold_bps: 13.0,
            },
        );

        let detail = GridProjector::new().project_detail(&source);

        assert_eq!(
            detail.execution.replacement_gate,
            Some(
                grid_protocol::ReplacementGateView::ImprovementBelowThreshold {
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

        let activity = GridProjector::new().project_activity(&source);

        assert_eq!(activity.len(), 1);
        assert_eq!(
            activity[0].message,
            "submit order superseded by newer grid state"
        );
        assert_eq!(activity[0].level, ActivityLevelView::Info);
    }

    #[test]
    fn project_activity_renders_replacement_gate_event_message() {
        let mut source = source_with_submitting_effect();
        source.recent_domain_events = vec![StoredDomainEvent {
            id: 1,
            grid_id: GridId::new("btc-core"),
            event: DomainEvent::ReplacementGateApplied {
                reason: grid_core::events::ReplacementGateReason::RoundedMatch,
            },
            created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 0).unwrap(),
        }];

        let activity = GridProjector::new().project_activity(&source);

        assert_eq!(activity.len(), 2);
        assert_eq!(
            activity[0].message,
            "replacement gate: candidate matches working order after rounding"
        );
        assert_eq!(activity[0].level, ActivityLevelView::Info);
    }

    fn source_with_submitting_effect() -> GridReadModel {
        GridReadModel {
            grid_id: "btc-core".into(),
            venue: "binance".into(),
            symbol: "BTCUSDT".into(),
            status: GridStatus::Active,
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            lower_price: 90.0,
            upper_price: 110.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
            reference_price: Some(101.25),
            current_exposure: 3.5,
            target_exposure: Some(4.0),
            realized_pnl_cumulative: 980.1,
            unrealized_pnl: 265.2,
            executor_mode: ExecutionMode::Passive,
            inventory_gap: 0.5,
            gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 26, 10, 0, 0).unwrap()),
            max_inventory_gap_abs: 1.5,
            max_gap_age_ms: 120_000,
            stats_started_at: Utc.with_ymd_and_hms(2026, 3, 26, 9, 45, 0).unwrap(),
            has_recovery_anomaly: false,
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
            recent_domain_events: Vec::new(),
            recent_effects: vec![test_effect(EffectStatus::Executing, None)],
        }
    }

    fn source_with_failed_effect_and_recent_event() -> GridReadModel {
        GridReadModel {
            recent_domain_events: vec![StoredDomainEvent {
                id: 1,
                grid_id: GridId::new("btc-core"),
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

    fn test_effect(status: EffectStatus, last_error: Option<String>) -> PersistedGridEffect {
        PersistedGridEffect {
            effect_id: "btc-core:batch-1:0".into(),
            grid_id: GridId::new("btc-core"),
            batch_id: "batch-1".into(),
            sequence: 0,
            effect: GridEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    side: Side::Buy,
                    price: 100.5,
                    quantity: 0.1,
                    client_order_id: "client-1".into(),
                    reduce_only: false,
                },
                target_exposure: Exposure(4.0),
            },
            status,
            attempt_count: u32::from(matches!(status, EffectStatus::Failed)),
            last_error,
            created_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
        }
    }

}
