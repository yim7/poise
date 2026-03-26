pub use grid_protocol::*;

#[cfg(test)]
mod tests {
    use super::{
        ExecutionStateView, GridDetailView, GridListResponse, GridStreamEvent, GridStreamPayload,
    };

    #[test]
    fn deserializes_grid_list_response() {
        let response: GridListResponse =
            serde_json::from_str(include_str!("../tests/fixtures/grid_list_response.json"))
                .unwrap();

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].id, "btc-core");
        assert_eq!(response.items[0].instrument.symbol, "BTCUSDT");
    }

    #[test]
    fn deserializes_grid_detail_view() {
        let detail: GridDetailView =
            serde_json::from_str(include_str!("../tests/fixtures/grid_detail_view.json")).unwrap();

        assert_eq!(detail.identity.id, "btc-core");
        assert_eq!(detail.execution.state, ExecutionStateView::Open);
        assert!(!detail.available_commands.is_empty());
    }

    #[test]
    fn deserializes_grid_stream_list_item_changed() {
        let event: GridStreamEvent = serde_json::from_str(
            include_str!("../tests/fixtures/ws_grid_list_item_changed.json"),
        )
        .unwrap();

        assert_eq!(event.grid_id, "btc-core");
        assert!(matches!(
            event.payload,
            GridStreamPayload::GridListItemChanged { .. }
        ));
    }

    #[test]
    fn deserializes_grid_stream_detail_changed() {
        let event: GridStreamEvent = serde_json::from_str(
            include_str!("../tests/fixtures/ws_grid_detail_changed.json"),
        )
        .unwrap();

        assert_eq!(event.grid_id, "btc-core");
        assert!(matches!(event.payload, GridStreamPayload::GridDetailChanged { .. }));
    }
}
