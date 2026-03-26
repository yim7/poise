pub use grid_protocol::{
    CommandRequest, CommandResponse, GridSnapshot, GridStatus, GridSummary, WsEvent,
};

#[cfg(test)]
pub use grid_protocol::{
    ActivityLevelView, BandBoundary, DomainEvent, ExecutionStateView, GridCommandRequest,
    GridCommandType, GridConfig, GridDetailView, GridListResponse, GridStreamEvent,
    GridStreamPayload, OrderStatus, OutOfBandPolicy, PendingOrder, ShapeFamily, Side,
};

#[cfg(test)]
mod tests {
    use super::{
        ActivityLevelView, CommandResponse, DomainEvent, ExecutionStateView, GridCommandRequest,
        GridCommandType, GridDetailView, GridListResponse, GridSnapshot, GridStatus,
        GridStreamEvent, GridStreamPayload, GridSummary, WsEvent,
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
        let event: GridStreamEvent = serde_json::from_str(
            include_str!("../tests/fixtures/ws_grid_list_item_changed.json"),
        )
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
        let event: GridStreamEvent = serde_json::from_str(
            include_str!("../tests/fixtures/ws_grid_detail_changed.json"),
        )
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
    fn deserializes_legacy_grid_summary_list() {
        let grids: Vec<GridSummary> =
            serde_json::from_str(include_str!("../tests/fixtures/instance_summaries.json"))
                .unwrap();

        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].id, "btc-core");
        assert_eq!(grids[0].status, GridStatus::Active);
    }

    #[test]
    fn deserializes_legacy_grid_snapshot() {
        let snapshot: GridSnapshot =
            serde_json::from_str(include_str!("../tests/fixtures/instance_snapshot.json")).unwrap();

        assert_eq!(snapshot.id, "btc-core");
        assert_eq!(snapshot.symbol, "BTCUSDT");
        assert_eq!(snapshot.status, GridStatus::Holding);
        assert_eq!(snapshot.current_exposure, 3.5);
    }

    #[test]
    fn deserializes_legacy_ws_event() {
        let event: WsEvent =
            serde_json::from_str(include_str!("../tests/fixtures/ws_event.json")).unwrap();

        assert_eq!(event.grid_id, "btc-core");
        assert_eq!(
            event.event,
            DomainEvent::ExposureTargetChanged { from: 0.0, to: 4.0 }
        );
    }

    #[test]
    fn deserializes_legacy_snapshot_updated_ws_event() {
        let event: WsEvent = serde_json::from_str(
            r#"{
                "grid_id": "btc-core",
                "event": "snapshot_updated"
            }"#,
        )
        .unwrap();

        assert_eq!(event.grid_id, "btc-core");
        assert_eq!(event.event, DomainEvent::SnapshotUpdated);
    }

    #[test]
    fn deserializes_legacy_command_response() {
        let response: CommandResponse =
            serde_json::from_str(include_str!("../tests/fixtures/command_response.json")).unwrap();

        assert_eq!(response.grid_id, "btc-core");
        assert!(response.accepted);
    }
}
