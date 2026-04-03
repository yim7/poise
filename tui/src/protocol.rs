#[allow(unused_imports)]
pub use poise_protocol::{
    ActivityLevelView, ExecutionIntentView, ExecutionSlotPhaseView, ExecutionStateView,
    ExecutionStatusView, GridCommandType, GridCommandView, GridExecutionView, GridStatisticsView,
    GridStatus, ReplacementGateView, TrackCommandAccepted, TrackCommandRequest, TrackDetailView,
    TrackDiagnosticsView, TrackListItemView, TrackListResponse, TrackStreamEvent,
    TrackStreamPayload,
};

#[cfg(test)]
pub use poise_protocol::{ExecutionBadgeView, ExposureSummaryView};

#[cfg(test)]
mod tests {
    use super::{
        ActivityLevelView, ExecutionStateView, ExecutionStatusView, GridCommandType,
        TrackCommandAccepted, TrackCommandRequest, TrackDetailView, TrackListResponse,
        TrackDiagnosticsView, TrackStreamEvent, TrackStreamPayload,
    };

    #[test]
    fn deserializes_grid_list_response() {
        let response: TrackListResponse =
            serde_json::from_str(include_str!("../tests/fixtures/track_list_response.json"))
                .unwrap();

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].id, "btc-core");
        assert_eq!(response.items[0].instrument.symbol, "BTCUSDT");
        assert_eq!(response.items[0].instrument.venue, "binance_futures");
    }

    #[test]
    fn deserializes_grid_detail_view() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../tests/fixtures/track_detail_view.json")).unwrap();

        assert_eq!(detail.identity.id, "btc-core");
        assert_eq!(detail.identity.instrument.venue, "binance_futures");
        assert!((detail.statistics.realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((detail.statistics.total_pnl - 1245.3).abs() < f64::EPSILON);
        assert_eq!(detail.execution.state, ExecutionStateView::Open);
        assert_eq!(
            detail.execution.execution_status,
            ExecutionStatusView::Normal
        );
        assert!(detail.execution.attention_reasons.is_empty());
        assert_eq!(detail.execution.active_slot_count, 1);
        assert_eq!(detail.execution.slots.len(), 1);
        assert_eq!(detail.activity[0].level, ActivityLevelView::Info);
        assert_eq!(detail.available_commands[0].command, GridCommandType::Pause);
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
            "target exposure 3.5000 -> 4.0000"
        );
    }

    #[test]
    fn deserializes_grid_detail_view_without_statistics() {
        let detail: TrackDetailView = serde_json::from_str(
            r#"{
                "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                "status":{"lifecycle":{"status":"active","updated_at":"2026-03-28T12:34:56Z"},"reference_price":64000.0},
                "strategy":{"lower_price":60000.0,"upper_price":68000.0,"shape_family":"linear","out_of_band_policy":"freeze"},
                "market":{"mark_price":64123.4,"index_price":64120.1},
                "position":{"current_exposure":0.5,"target_exposure":0.75},
                "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                "activity":[{"ts":"2026-03-28T12:34:56Z","message":"Track activated","level":"info"}],
                "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
            }"#,
        )
        .unwrap();

        assert_eq!(detail.identity.id, "btc-core");
        assert!((detail.statistics.realized_pnl - 0.0).abs() < f64::EPSILON);
        assert!((detail.statistics.total_pnl - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn deserializes_grid_stream_list_item_changed() {
        let event: TrackStreamEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/ws_track_list_item_changed.json"
        ))
        .unwrap();

        assert_eq!(event.track_id, "btc-core");
        match event.payload {
            TrackStreamPayload::TrackListItemChanged { item } => {
                assert_eq!(item.instrument.venue, "binance_futures");
                assert_eq!(item.execution.execution_status, ExecutionStatusView::Normal);
                assert_eq!(item.execution.active_slot_count, 0);
            }
            _ => panic!("unexpected payload variant"),
        }
    }

    #[test]
    fn deserializes_grid_stream_detail_changed() {
        let event: TrackStreamEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/ws_track_detail_changed.json"
        ))
        .unwrap();

        assert_eq!(event.track_id, "btc-core");
        match event.payload {
            TrackStreamPayload::TrackDetailChanged { detail } => {
                assert_eq!(detail.identity.instrument.symbol, "BTCUSDT");
                assert!((detail.statistics.realized_pnl - 980.1).abs() < f64::EPSILON);
                assert!((detail.statistics.total_pnl - 1245.3).abs() < f64::EPSILON);
                assert_eq!(detail.available_commands[0].command, GridCommandType::Pause);
            }
            _ => panic!("unexpected payload variant"),
        }
    }

    #[test]
    fn deserializes_grid_stream_detail_changed_without_statistics() {
        let event: TrackStreamEvent = serde_json::from_str(
            r#"{
                "track_id":"btc-core",
                "payload":{
                    "type":"track_detail_changed",
                    "detail":{
                        "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                        "status":{"lifecycle":{"status":"active","updated_at":"2026-03-28T12:34:56Z"},"reference_price":64000.0},
                        "strategy":{"lower_price":60000.0,"upper_price":68000.0,"shape_family":"linear","out_of_band_policy":"freeze"},
                        "market":{"mark_price":64123.4,"index_price":64120.1},
                        "position":{"current_exposure":0.5,"target_exposure":0.75},
                        "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                        "activity":[{"ts":"2026-03-28T12:34:56Z","message":"Track activated","level":"info"}],
                        "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
                    }
                }
            }"#,
        )
        .unwrap();

        match event.payload {
            TrackStreamPayload::TrackDetailChanged { detail } => {
                assert_eq!(detail.identity.id, "btc-core");
                assert!((detail.statistics.realized_pnl - 0.0).abs() < f64::EPSILON);
                assert!((detail.statistics.total_pnl - 0.0).abs() < f64::EPSILON);
            }
            _ => panic!("unexpected payload variant"),
        }
    }

    #[test]
    fn deserializes_grid_command_request() {
        let request: TrackCommandRequest = serde_json::from_str(r#"{"command":"pause"}"#).unwrap();

        assert_eq!(request.command, GridCommandType::Pause);
    }

    #[test]
    fn deserializes_grid_command_accepted() {
        let response: TrackCommandAccepted =
            serde_json::from_str(r#"{"track_id":"btc-core","command":"pause","accepted":true}"#)
                .unwrap();

        assert_eq!(response.track_id, "btc-core");
        assert_eq!(response.command, GridCommandType::Pause);
        assert!(response.accepted);
    }
}
