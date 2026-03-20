use grid_platform_service::{
    execution::simulate_paper_fills,
    protocol::{OpenOrder, RuntimeSnapshot},
};

#[test]
fn paper_fill_simulation_crosses_buy_order_and_updates_long_position() {
    let mut snapshot = flat_snapshot();
    snapshot.execution.open_orders =
        vec![open_order("ord_buy_01", "grid_buy_01", "buy", 100.0, 1.0)];
    snapshot.runtime.last_price = 99.5;
    snapshot.runtime.mark_price = 99.5;

    let patch = simulate_paper_fills(&snapshot, "2025-01-01T00:00:01Z");

    assert_eq!(patch.recent_fills.len(), 1);
    assert_eq!(patch.recent_fills[0].order_id, "ord_buy_01");
    assert_eq!(
        patch.recent_fills[0].client_order_id.as_deref(),
        Some("grid_buy_01")
    );
    assert_eq!(patch.recent_fills[0].price, 100.0);
    assert!(
        patch
            .open_orders
            .as_ref()
            .is_some_and(|orders| orders.is_empty())
    );
    assert_eq!(patch.runtime_patch.position_qty, Some(1.0));
    assert_eq!(patch.runtime_patch.position_avg_price, Some(100.0));
    assert_eq!(patch.runtime_patch.realized_pnl, Some(0.0));
    assert_eq!(patch.runtime_patch.unrealized_pnl, Some(-0.5));
}

#[test]
fn paper_fill_simulation_realizes_pnl_when_sell_closes_long_position() {
    let mut snapshot = flat_snapshot();
    snapshot.runtime.position_qty = 1.0;
    snapshot.runtime.position_avg_price = 100.0;
    snapshot.runtime.realized_pnl = 2.0;
    snapshot.execution.open_orders = vec![open_order(
        "ord_sell_01",
        "grid_sell_01",
        "sell",
        105.0,
        1.0,
    )];
    snapshot.runtime.last_price = 105.5;
    snapshot.runtime.mark_price = 105.5;

    let patch = simulate_paper_fills(&snapshot, "2025-01-01T00:00:02Z");

    assert_eq!(patch.recent_fills.len(), 1);
    assert_eq!(patch.recent_fills[0].order_id, "ord_sell_01");
    assert_eq!(patch.recent_fills[0].price, 105.0);
    assert!(
        patch
            .open_orders
            .as_ref()
            .is_some_and(|orders| orders.is_empty())
    );
    assert_eq!(patch.runtime_patch.position_qty, Some(0.0));
    assert_eq!(patch.runtime_patch.position_avg_price, Some(0.0));
    assert_eq!(patch.runtime_patch.realized_pnl, Some(7.0));
    assert_eq!(patch.runtime_patch.unrealized_pnl, Some(0.0));
}

fn flat_snapshot() -> RuntimeSnapshot {
    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.runtime.strategy_state = "paused".into();
    snapshot.runtime.last_price = 100.0;
    snapshot.runtime.mark_price = 100.0;
    snapshot.runtime.position_qty = 0.0;
    snapshot.runtime.position_avg_price = 0.0;
    snapshot.runtime.unrealized_pnl = 0.0;
    snapshot.runtime.realized_pnl = 0.0;
    snapshot.execution.open_orders.clear();
    snapshot.execution.recent_fills.clear();
    snapshot.execution.pending_commands.clear();
    snapshot.execution.last_command_ack = None;
    snapshot.execution.last_command_ack_event = None;
    snapshot.execution.recent_commands.clear();
    snapshot
}

fn open_order(
    order_id: &str,
    client_order_id: &str,
    side: &str,
    price: f64,
    qty: f64,
) -> OpenOrder {
    OpenOrder {
        order_id: order_id.into(),
        client_order_id: client_order_id.into(),
        side: side.into(),
        price,
        qty,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }
}
