use poise_engine::ports::ExchangeOpenOrderSnapshot;
use poise_engine::runtime::{BindingView, TrackRuntimeView};

pub(super) fn describe_runtime_bindings(runtime: Option<&TrackRuntimeView>) -> Vec<String> {
    let Some(runtime) = runtime else {
        return Vec::new();
    };
    runtime
        .executor
        .bindings
        .iter()
        .map(describe_binding)
        .collect()
}

pub(super) fn describe_open_orders(open_orders: &ExchangeOpenOrderSnapshot) -> Vec<String> {
    open_orders
        .orders()
        .iter()
        .map(|order| {
            format!(
                "{} {:?} {} qty {:.4} @ {:.4} status {:?} order_id={} client_order_id={}",
                order.instrument.symbol,
                order.side,
                if order.side == poise_core::types::Side::Buy {
                    "increase_or_bid"
                } else {
                    "decrease_or_ask"
                },
                order.qty,
                order.price,
                order.status,
                order.order_id,
                order.client_order_id
            )
        })
        .collect()
}

fn describe_binding(binding: &BindingView) -> String {
    format!(
        "{:?} {:?} {:?} qty {:.4} @ {:.4} binding_id={}",
        binding.policy, binding.status, binding.side, binding.quantity, binding.price, binding.id
    )
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::ports::{ExchangeOpenOrderSnapshot, ExchangeOrder, OrderStatus};
    use poise_engine::runtime::{
        BindingView, ExecutorView, StrategyPriceStatus, TrackRuntimeView, TrackStatus,
    };

    use super::{describe_open_orders, describe_runtime_bindings};

    #[test]
    fn describe_runtime_bindings_includes_binding_identity_and_order_shape() {
        let runtime = TrackRuntimeView {
            status: TrackStatus::Active,
            current_exposure: Exposure(0.0),
            position_qty: 0.0,
            desired_exposure: None,
            execution_target_exposure: None,
            risk_acquisition: Default::default(),
            manual_target_override: None,
            executor: ExecutorView {
                bindings: vec![BindingView {
                    id: "binding-1".to_string(),
                    policy: poise_engine::executor::PolicyKind::CurveMaker,
                    is_passive_execution: true,
                    status: poise_engine::executor::BindingStatus::Working,
                    side: Side::Buy,
                    price: 4670.0,
                    quantity: 0.066,
                    increases_inventory: true,
                }],
                recovery_anomaly: None,
            },
            pnl_stats: Default::default(),
            unrealized_pnl: 0.0,
            has_account_margin_guard: false,
            price_execution_block_reason: None,
            strategy_price: Some(4680.0),
            strategy_price_status: StrategyPriceStatus::Live,
            mark_price: Some(4680.0),
            best_bid: Some(4679.5),
            best_ask: Some(4680.5),
            last_tick_at: Some(Utc::now()),
            market_data_stale_since: None,
        };

        let described = describe_runtime_bindings(Some(&runtime));

        assert_eq!(described.len(), 1);
        assert!(described[0].contains("CurveMaker"));
        assert!(described[0].contains("Working"));
        assert!(described[0].contains("Buy"));
        assert!(described[0].contains("4670.0000"));
        assert!(described[0].contains("0.0660"));
        assert!(described[0].contains("binding-1"));
    }

    #[test]
    fn describe_open_orders_includes_order_and_client_order_identity() {
        let open_orders = ExchangeOpenOrderSnapshot::from_complete_exchange_query(vec![
            ExchangeOrder {
                instrument: Instrument::new(Venue::Binance, "ETHUSDT"),
                order_id: "order-4670".to_string(),
                client_order_id: "client-4670".to_string(),
                side: Side::Buy,
                price: 4670.0,
                qty: 0.066,
                filled_qty: 0.0,
                status: OrderStatus::New,
            },
            ExchangeOrder {
                instrument: Instrument::new(Venue::Binance, "ETHUSDT"),
                order_id: "order-4640".to_string(),
                client_order_id: "client-4640".to_string(),
                side: Side::Buy,
                price: 4640.0,
                qty: 0.066,
                filled_qty: 0.0,
                status: OrderStatus::New,
            },
        ]);

        let described = describe_open_orders(&open_orders);

        assert_eq!(described.len(), 2);
        assert!(described[0].contains("ETHUSDT"));
        assert!(described[0].contains("4670.0000"));
        assert!(described[0].contains("order-4670"));
        assert!(described[0].contains("client-4670"));
        assert!(described[1].contains("4640.0000"));
        assert!(described[1].contains("order-4640"));
        assert!(described[1].contains("client-4640"));
    }
}
