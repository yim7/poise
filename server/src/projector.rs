use grid_core::events::DomainEvent;
use grid_engine::ports::{EffectStatus, OrderStatus as EngineOrderStatus};
use grid_engine::runtime::GridStatus as EngineGridStatus;
use grid_engine::transition::GridEffect;
use grid_protocol::{
    ActivityLevelView, ExecutionBadgeView, ExecutionStateView, ExposureSummaryView,
    GridActivityItemView, GridCommandType, GridCommandView, GridDetailView, GridExecutionView,
    GridIdentityView, GridLifecycleView, GridListItemView, GridMarketView, GridPositionView,
    GridStatisticsView, GridStatus as ProtocolGridStatus, GridStatusPanelView, GridStrategyView,
    InstrumentView, OrderExecutionView, OrderStatus as ProtocolOrderStatus,
    OutOfBandPolicy as ProtocolPolicy, ShapeFamily as ProtocolShapeFamily,
    Side as ProtocolSide,
};

use crate::query_service::GridReadModelSource;

pub struct GridProjector;

impl GridProjector {
    pub fn new() -> Self {
        Self
    }

    pub fn project_list_item(&self, source: &GridReadModelSource) -> GridListItemView {
        GridListItemView {
            id: source.snapshot.grid_id.as_str().to_string(),
            instrument: project_instrument(&source.snapshot.instrument),
            lifecycle: GridLifecycleView {
                status: project_grid_status(&source.snapshot.status),
                updated_at: source.snapshot_updated_at.to_rfc3339(),
            },
            reference_price: source.snapshot.observed.reference_price,
            exposure: ExposureSummaryView {
                current: source.snapshot.current_exposure.0,
                target: source
                    .snapshot
                    .target_exposure
                    .as_ref()
                    .map(|value| value.0),
            },
            execution: ExecutionBadgeView {
                state: project_execution_state(source),
                pending_order_count: pending_order_count(source),
            },
        }
    }

    pub fn project_detail(&self, source: &GridReadModelSource) -> GridDetailView {
        GridDetailView {
            identity: GridIdentityView {
                id: source.snapshot.grid_id.as_str().to_string(),
                instrument: project_instrument(&source.snapshot.instrument),
            },
            status: GridStatusPanelView {
                lifecycle: GridLifecycleView {
                    status: project_grid_status(&source.snapshot.status),
                    updated_at: source.snapshot_updated_at.to_rfc3339(),
                },
                reference_price: source.snapshot.observed.reference_price,
            },
            strategy: GridStrategyView {
                lower_price: source.snapshot.config.lower_price,
                upper_price: source.snapshot.config.upper_price,
                shape_family: project_shape_family(source.snapshot.config.shape_family),
                out_of_band_policy: project_out_of_band_policy(
                    source.snapshot.config.out_of_band_policy,
                ),
            },
            market: GridMarketView {
                mark_price: source.snapshot.observed.reference_price,
                index_price: source.snapshot.observed.reference_price,
            },
            position: GridPositionView {
                current_exposure: source.snapshot.current_exposure.0,
                target_exposure: source
                    .snapshot
                    .target_exposure
                    .as_ref()
                    .map(|value| value.0),
            },
            statistics: GridStatisticsView {
                total_pnl: source.snapshot.risk.realized_pnl_cumulative
                    + source.snapshot.risk.unrealized_pnl,
                realized_pnl: source.snapshot.risk.realized_pnl_cumulative,
            },
            execution: GridExecutionView {
                state: project_execution_state(source),
                pending_order: source.snapshot.pending_order.as_ref().map(|order| {
                    OrderExecutionView {
                        symbol: source.snapshot.instrument.symbol.clone(),
                        order_id: order.order_id.clone(),
                        side: project_side(order.side),
                        price: order.price,
                        quantity: order.quantity,
                        status: project_order_status(order.status),
                    }
                }),
            },
            activity: self.project_activity(source),
            available_commands: project_available_commands(&source.snapshot.status),
        }
    }

    pub fn project_activity(&self, source: &GridReadModelSource) -> Vec<GridActivityItemView> {
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

fn project_instrument(value: &grid_engine::grid::Instrument) -> InstrumentView {
    InstrumentView {
        venue: match value.venue {
            grid_engine::grid::Venue::Binance => "binance_futures".to_string(),
        },
        symbol: value.symbol.clone(),
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

fn project_order_status(value: EngineOrderStatus) -> ProtocolOrderStatus {
    match value {
        EngineOrderStatus::Submitting => ProtocolOrderStatus::Submitting,
        EngineOrderStatus::New => ProtocolOrderStatus::New,
        EngineOrderStatus::PartiallyFilled => ProtocolOrderStatus::PartiallyFilled,
        EngineOrderStatus::Filled => ProtocolOrderStatus::Filled,
        EngineOrderStatus::Canceling => ProtocolOrderStatus::Canceling,
        EngineOrderStatus::Canceled => ProtocolOrderStatus::Canceled,
        EngineOrderStatus::Rejected => ProtocolOrderStatus::Rejected,
        EngineOrderStatus::Expired => ProtocolOrderStatus::Expired,
    }
}

fn project_execution_state(source: &GridReadModelSource) -> ExecutionStateView {
    match source.snapshot.status {
        EngineGridStatus::Paused => ExecutionStateView::Paused,
        EngineGridStatus::Terminated => ExecutionStateView::Closed,
        _ => ExecutionStateView::Open,
    }
}

fn pending_order_count(source: &GridReadModelSource) -> u32 {
    u32::from(
        source.snapshot.pending_order.is_some()
            || source.recent_effects.iter().any(|effect| {
                matches!(
                    effect.status,
                    EffectStatus::Pending | EffectStatus::Executing
                )
            }),
    )
}

fn project_available_commands(status: &EngineGridStatus) -> Vec<GridCommandView> {
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
            enabled: matches!(status, EngineGridStatus::Paused),
            disabled_reason: match status {
                EngineGridStatus::Paused => None,
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
            enabled: false,
            disabled_reason: Some("flatten command is not implemented".into()),
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
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{Exposure, Side};
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::ports::{
        EffectStatus, OrderRequest, OrderStatus, PersistedGridEffect, StoredDomainEvent,
    };
    use grid_engine::runtime::{GridStatus, PendingOrder, RiskState};
    use grid_engine::snapshot::{GridRuntimeSnapshot, ObservedState};
    use grid_engine::transition::GridEffect;
    use grid_protocol::{ActivityLevelView, ExecutionStateView, GridCommandType};

    use super::GridProjector;
    use crate::query_service::GridReadModelSource;

    #[test]
    fn project_list_item_summarizes_execution_state() {
        let source = source_with_submitting_effect();
        let item = GridProjector::new().project_list_item(&source);

        assert_eq!(item.id, "btc-core");
        assert_eq!(item.execution.state, ExecutionStateView::Open);
        assert_eq!(item.execution.pending_order_count, 1);
        assert_eq!(item.lifecycle.updated_at, "2026-03-26T10:01:30+00:00");
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
        assert!(!detail.available_commands[3].enabled);
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
            detail_json["execution"]["pending_order"]
                .get("client_order_id")
                .is_none()
        );
    }

    #[test]
    fn project_detail_projects_statistics_from_risk_state() {
        let detail = GridProjector::new().project_detail(&source_with_submitting_effect());

        assert!((detail.statistics.realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((detail.statistics.total_pnl - 1245.3).abs() < f64::EPSILON);
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

    fn source_with_submitting_effect() -> GridReadModelSource {
        GridReadModelSource {
            snapshot: test_snapshot(),
            snapshot_updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
            recent_domain_events: Vec::new(),
            recent_effects: vec![test_effect(EffectStatus::Executing, None)],
        }
    }

    fn source_with_failed_effect_and_recent_event() -> GridReadModelSource {
        GridReadModelSource {
            snapshot: test_snapshot(),
            snapshot_updated_at: Utc.with_ymd_and_hms(2026, 3, 26, 10, 1, 30).unwrap(),
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

    fn test_snapshot() -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: GridId::new("btc-core"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            status: GridStatus::Active,
            current_exposure: Exposure(3.5),
            target_exposure: Some(Exposure(4.0)),
            pending_order: Some(PendingOrder {
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 100.5,
                quantity: 0.1,
                target_exposure: Exposure(4.0),
                status: OrderStatus::New,
            }),
            risk: RiskState {
                realized_pnl_day: None,
                realized_pnl_today: 0.0,
                realized_pnl_cumulative: 980.1,
                unrealized_pnl: 265.2,
            },
            observed: ObservedState {
                reference_price: Some(101.25),
                out_of_band_since: None,
            },
        }
    }
}
