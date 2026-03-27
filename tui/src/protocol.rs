pub use grid_protocol::{
    ActivityLevelView, ExecutionStateView, GridCommandAccepted, GridCommandRequest,
    GridCommandType, GridCommandView, GridDetailView, GridExecutionView, GridListItemView,
    GridListResponse, GridStatus, GridStreamEvent, GridStreamPayload, ReplacementGateView,
};

#[cfg(test)]
pub use grid_protocol::{ExecutionBadgeView, ExposureSummaryView};

#[cfg(test)]
mod tests {
    use super::{
        ActivityLevelView, ExecutionStateView, GridCommandAccepted, GridCommandRequest,
        GridCommandType, GridDetailView, GridListResponse, GridStreamEvent, GridStreamPayload,
    };

    #[test]
    fn deserializes_grid_list_response() {
        let response: GridListResponse =
            serde_json::from_str(include_str!("../tests/fixtures/grid_list_response.json"))
                .unwrap();

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].id, "btc-core");
        assert_eq!(response.items[0].instrument.symbol, "BTCUSDT");
        assert_eq!(response.items[0].instrument.venue, "binance_futures");
    }

    #[test]
    fn deserializes_grid_detail_view() {
        let detail: GridDetailView =
            serde_json::from_str(include_str!("../tests/fixtures/grid_detail_view.json")).unwrap();

        assert_eq!(detail.identity.id, "btc-core");
        assert_eq!(detail.identity.instrument.venue, "binance_futures");
        assert_eq!(detail.execution.state, ExecutionStateView::Open);
        assert_eq!(detail.activity[0].level, ActivityLevelView::Info);
        assert_eq!(detail.available_commands[0].command, GridCommandType::Pause);
        assert!(!detail.available_commands.is_empty());
    }

    #[test]
    fn deserializes_grid_stream_list_item_changed() {
        let event: GridStreamEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/ws_grid_list_item_changed.json"
        ))
        .unwrap();

        assert_eq!(event.grid_id, "btc-core");
        match event.payload {
            GridStreamPayload::GridListItemChanged { item } => {
                assert_eq!(item.instrument.venue, "binance_futures");
                assert_eq!(item.execution.pending_order_count, 0);
            }
            _ => panic!("unexpected payload variant"),
        }
    }

    #[test]
    fn deserializes_grid_stream_detail_changed() {
        let event: GridStreamEvent = serde_json::from_str(include_str!(
            "../tests/fixtures/ws_grid_detail_changed.json"
        ))
        .unwrap();

        assert_eq!(event.grid_id, "btc-core");
        match event.payload {
            GridStreamPayload::GridDetailChanged { detail } => {
                assert_eq!(detail.identity.instrument.symbol, "BTCUSDT");
                assert_eq!(detail.available_commands[0].command, GridCommandType::Pause);
            }
            _ => panic!("unexpected payload variant"),
        }
    }

    #[test]
    fn deserializes_grid_command_request() {
        let request: GridCommandRequest = serde_json::from_str(r#"{"command":"pause"}"#).unwrap();

        assert_eq!(request.command, GridCommandType::Pause);
    }

    #[test]
    fn deserializes_grid_command_accepted() {
        let response: GridCommandAccepted =
            serde_json::from_str(r#"{"grid_id":"btc-core","command":"pause","accepted":true}"#)
                .unwrap();

        assert_eq!(response.grid_id, "btc-core");
        assert_eq!(response.command, GridCommandType::Pause);
        assert!(response.accepted);
    }
}
