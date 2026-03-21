use grid_platform_service::{
    protocol::{OpenOrder, RuntimeSnapshot},
    storage::PersistedRuntime,
};

pub fn seed_runtime_with_position_and_orders() -> PersistedRuntime {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::empty_bootstrap();
    runtime.snapshot.runtime.last_price = 2361.48;
    runtime.snapshot.runtime.mark_price = 2361.55;
    runtime.snapshot.runtime.position_qty = 0.25;
    runtime.snapshot.runtime.position_avg_price = 2354.2;
    runtime.snapshot.runtime.realized_pnl = 14.52;
    runtime.snapshot.execution.open_orders = vec![
        open_order("ord_1001", "grid_buy_01", "buy", 2352.8),
        open_order("ord_1002", "grid_sell_01", "sell", 2368.3),
    ];
    runtime
}

fn open_order(order_id: &str, client_order_id: &str, side: &str, price: f64) -> OpenOrder {
    OpenOrder {
        order_id: order_id.into(),
        client_order_id: client_order_id.into(),
        side: side.into(),
        price,
        qty: 0.10,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }
}
