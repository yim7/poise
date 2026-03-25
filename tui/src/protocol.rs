pub use grid_protocol::*;

#[cfg(test)]
mod tests {
    use super::{
        BandState, CommandResponse, DomainEvent, GridConfig, GridSnapshot,
        GridStatus, GridSummary, OutOfBandPolicy, PendingOrder, ShapeFamily, Side, WsEvent,
    };

    #[test]
    fn deserializes_grid_summary_list() {
        let grids: Vec<GridSummary> =
            serde_json::from_str(include_str!("../tests/fixtures/instance_summaries.json"))
                .unwrap();

        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].id, "BTCUSDT");
        assert_eq!(grids[0].status, GridStatus::Active);
        assert_eq!(grids[0].reference_price, Some(101.25));
    }

    #[test]
    fn deserializes_grid_snapshot() {
        let snapshot: GridSnapshot =
            serde_json::from_str(include_str!("../tests/fixtures/instance_snapshot.json")).unwrap();

        assert_eq!(snapshot.status, GridStatus::Holding);
        assert_eq!(snapshot.current_exposure, 3.5);
        assert_eq!(snapshot.config.shape_family, ShapeFamily::Linear);
    }

    #[test]
    fn deserializes_command_response() {
        let response: CommandResponse =
            serde_json::from_str(include_str!("../tests/fixtures/command_response.json")).unwrap();

        assert_eq!(response.grid_id, "BTCUSDT");
        assert!(response.accepted);
    }

    #[test]
    fn deserializes_ws_event() {
        let event: WsEvent =
            serde_json::from_str(include_str!("../tests/fixtures/ws_event.json")).unwrap();

        assert_eq!(event.grid_id, "BTCUSDT");
        assert_eq!(
            event.event,
            DomainEvent::ExposureTargetChanged {
                from: 0.0,
                to: 4.0,
            }
        );
    }

    #[test]
    fn band_state_uses_reference_price() {
        let snapshot = GridSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: GridStatus::Active,
            current_exposure: 1.0,
            target_exposure: Some(2.0),
            reference_price: Some(85.0),
            pending_order: Some(PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 85.0,
                quantity: 0.1,
                status: "NEW".into(),
            }),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        };

        assert_eq!(snapshot.band_state(), BandState::BelowBand);
    }
}
