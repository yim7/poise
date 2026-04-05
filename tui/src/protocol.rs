#[allow(unused_imports)]
pub use poise_protocol::{
    AccountSummaryView, ActivityLevelView, ExecutionIntentView, ExecutionSlotPhaseView,
    ExecutionStateView, ExecutionStatusView, ReplacementGateView, RiskSignalView, StreamEvent,
    TrackCommandAccepted, TrackCommandRequest, TrackCommandType, TrackCommandView, TrackDetailView,
    TrackDiagnosticsView, TrackExecutionStatsView, TrackExecutionView, TrackListItemView,
    TrackListPnlView, TrackListResponse, TrackPnlView, TrackStatus,
};

#[cfg(test)]
pub use poise_protocol::{ExecutionBadgeView, ExposureSummaryView};

#[cfg(test)]
mod tests {
    use super::{
        ActivityLevelView, ExecutionStateView, ExecutionStatusView, StreamEvent,
        TrackCommandAccepted, TrackCommandRequest, TrackCommandType, TrackDetailView,
        TrackDiagnosticsView, TrackListResponse,
    };

    #[test]
    fn deserializes_track_list_response() {
        let response: TrackListResponse =
            serde_json::from_str(include_str!("../tests/fixtures/track_list_response.json"))
                .unwrap();

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].id, "btc-core");
        assert_eq!(response.items[0].instrument.symbol, "BTCUSDT");
        assert_eq!(response.items[0].instrument.venue, "binance_futures");
    }

    #[test]
    fn deserializes_track_detail_view() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../tests/fixtures/track_detail_view.json")).unwrap();
        let detail_json = serde_json::to_value(&detail).unwrap();

        assert_eq!(detail.identity.id, "btc-core");
        assert_eq!(detail.identity.instrument.venue, "binance_futures");
        assert!((detail.pnl.realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((detail.pnl.total_pnl - 1245.3).abs() < f64::EPSILON);
        assert!((detail.pnl.unrealized_pnl - 265.2).abs() < f64::EPSILON);
        assert!((detail.execution_stats.max_inventory_gap_abs - 1.5).abs() < f64::EPSILON);
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
        assert_eq!(detail.execution.state, ExecutionStateView::Open);
        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert!(detail.execution.attention_reasons.is_empty());
        assert_eq!(detail.execution.active_slot_count, 1);
        assert_eq!(detail.execution.slots.len(), 1);
        assert_eq!(detail.activity[0].level, ActivityLevelView::Info);
        assert_eq!(
            detail.available_commands[0].command,
            TrackCommandType::Pause
        );
        assert!(!detail.available_commands.is_empty());
    }

    #[test]
    fn deserializes_track_diagnostics_view_fixture() {
        let diagnostics: TrackDiagnosticsView = serde_json::from_str(include_str!(
            "../tests/fixtures/track_diagnostics_view.json"
        ))
        .unwrap();

        assert_eq!(diagnostics.items.len(), 1);
        assert_eq!(
            diagnostics.items[0].message,
            "desired exposure 3.5000 -> 4.0000"
        );
    }

    #[test]
    fn rejects_track_detail_view_without_pnl_and_execution_stats() {
        let result = serde_json::from_str::<TrackDetailView>(
            r#"{
                "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                "status":{"lifecycle":{"status":"active","updated_at":"2026-03-28T12:34:56Z"},"reference_price":64000.0},
                "strategy":{"lower_price":60000.0,"upper_price":68000.0,"long_exposure_units":8.0,"short_exposure_units":8.0,"notional_per_unit":375.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":"freeze"},
                "market":{"mark_price":64123.4,"index_price":64120.1},
                "position":{"current_exposure":0.5,"desired_exposure":0.75},
                "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                "activity":[{"ts":"2026-03-28T12:34:56Z","message":"Track activated","level":"info"}],
                    "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
                }"#,
        );

        assert!(result.is_err());
    }

    #[test]
    fn deserializes_track_stream_list_item_changed() {
        let event: StreamEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/ws_track_list_item_changed.json"
        ))
        .unwrap();

        match event {
            StreamEvent::TrackListItemChanged { track_id, item } => {
                assert_eq!(track_id, "btc-core");
                assert_eq!(item.instrument.venue, "binance_futures");
                assert_eq!(item.execution.execution_status, ExecutionStatusView::Normal);
                assert_eq!(item.execution.active_slot_count, 0);
            }
            other => panic!("unexpected event variant: {other:?}"),
        }
    }

    #[test]
    fn deserializes_track_stream_detail_changed() {
        let event: StreamEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/ws_track_detail_changed.json"
        ))
        .unwrap();

        match event {
            StreamEvent::TrackDetailChanged { track_id, detail } => {
                assert_eq!(track_id, "btc-core");
                let detail_json = serde_json::to_value(&detail).unwrap();
                assert_eq!(detail.identity.instrument.symbol, "BTCUSDT");
                assert!((detail.pnl.realized_pnl - 980.1).abs() < f64::EPSILON);
                assert!((detail.pnl.total_pnl - 1245.3).abs() < f64::EPSILON);
                assert!((detail.pnl.unrealized_pnl - 265.2).abs() < f64::EPSILON);
                assert!((detail.execution_stats.max_inventory_gap_abs - 1.5).abs() < f64::EPSILON);
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
                    detail.available_commands[0].command,
                    TrackCommandType::Pause
                );
            }
            other => panic!("unexpected event variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_track_stream_detail_changed_without_pnl_and_execution_stats() {
        let result = serde_json::from_str::<StreamEvent>(
            r#"{
                "type":"track_detail_changed",
                "track_id":"btc-core",
                "detail":{
                    "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                    "status":{"lifecycle":{"status":"active","updated_at":"2026-03-28T12:34:56Z"},"reference_price":64000.0},
                    "strategy":{"lower_price":60000.0,"upper_price":68000.0,"long_exposure_units":8.0,"short_exposure_units":8.0,"notional_per_unit":375.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":"freeze"},
                    "market":{"mark_price":64123.4,"index_price":64120.1},
                    "position":{"current_exposure":0.5,"desired_exposure":0.75},
                    "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                    "activity":[{"ts":"2026-03-28T12:34:56Z","message":"Track activated","level":"info"}],
                    "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
                }
            }"#,
        );

        assert!(result.is_err());
    }

    #[test]
    fn deserializes_track_command_request() {
        let request: TrackCommandRequest = serde_json::from_str(r#"{"command":"pause"}"#).unwrap();

        assert_eq!(request.command, TrackCommandType::Pause);
    }

    #[test]
    fn deserializes_track_command_accepted() {
        let response: TrackCommandAccepted =
            serde_json::from_str(r#"{"track_id":"btc-core","command":"pause","accepted":true}"#)
                .unwrap();

        assert_eq!(response.track_id, "btc-core");
        assert_eq!(response.command, TrackCommandType::Pause);
        assert!(response.accepted);
    }
}
