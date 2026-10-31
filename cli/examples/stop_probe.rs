//! 手工核对用：在测试网上试一遍止损触发单（兜底单），看交易所实际怎么回。
//!
//! 只接受测试网实例。先用一张立即成交的小额买单开一点多仓，然后：
//! 1. 挂一张远低于标记价的止损卖单，看回执、挂单列表、按 `cloid` 查到的状态；
//! 2. 撤掉它，再查一次；
//! 3. 挂一张数量不足最低金额的止损单，看交易所拒不拒；
//! 4. 挂一张触发价高于标记价的止损卖单，看它是不是立即触发、成交价和成交流水；
//! 5. 平掉剩下的仓位。
//!
//! ```text
//! cargo run -p poise-cli --example stop_probe -- ~/.poise/testnet
//! ```

use std::path::PathBuf;

use chrono::{Duration, Utc};
use poise_hyperliquid::{Hyperliquid, HyperliquidConfig};
use poise_runtime::{
    Exchange, Network, OrderLookup, OrderRequest, StartupFile, StopRequest, TimeInForce,
};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, Side};
use poise_strategy::units::{Position, Price, Quantity};

fn new_id() -> ClientOrderId {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default() as u128;
    ClientOrderId::try_new(format!("0x{:032x}", nanos ^ 0x5eed_0000_0000_0000_0000)).unwrap()
}

/// 价格按 5 位有效数字、整数规整（BTC 的报价精度在这个量级上是整数）。
fn price(value: f64) -> Price {
    let digits = value.log10().floor() as i32 + 1;
    let step = 10f64.powi((digits - 5).max(0));
    Price::parse(&format!("{}", (value / step).round() * step)).unwrap()
}

fn as_f64(price: Price) -> f64 {
    price.to_string().parse().unwrap()
}

#[tokio::main]
async fn main() {
    let directory = PathBuf::from(std::env::args().nth(1).expect("instance directory"));
    let startup = StartupFile::load(directory.join("poise.toml")).expect("startup file");
    assert_eq!(startup.network(), Network::Testnet, "testnet only");
    let exchange = Hyperliquid::new(HyperliquidConfig {
        network: startup.network(),
        account_address: startup.account_address(),
        api_wallet_private_key: startup.api_wallet_private_key().clone(),
        base_url: None,
    })
    .unwrap();
    let btc = Instrument::try_new("BTC").unwrap();
    let mark = exchange.mark_price(&btc).await.unwrap();
    let before = exchange.position(&btc).await.unwrap();
    println!("mark {mark} position {before}");

    // 0. 开一点多仓：约 15 美元。
    let size = Quantity::parse(&format!("{:.5}", 15.0 / as_f64(mark))).unwrap();
    let opened = exchange
        .place_order(&OrderRequest {
            instrument: btc.clone(),
            client_order_id: new_id(),
            side: Side::Buy,
            limit_price: price(as_f64(mark) * 1.01),
            quantity: size,
            reduce_only: false,
            time_in_force: TimeInForce::ImmediateOrCancel,
            expires_at: Utc::now() + Duration::seconds(10),
        })
        .await;
    println!("open {size}: {opened:?}");
    let position = exchange.position(&btc).await.unwrap();
    println!("position {position}");
    let held = Quantity::from_units(position.units().max(0));

    // 1. 远低于标记价的止损卖单。
    let far = new_id();
    let trigger = price(as_f64(mark) * 0.8);
    let placed = exchange
        .place_stop(&StopRequest {
            instrument: btc.clone(),
            client_order_id: far.clone(),
            side: Side::Sell,
            trigger_price: trigger,
            limit_price: price(as_f64(trigger) * 0.9),
            quantity: held,
            expires_at: Utc::now() + Duration::seconds(10),
        })
        .await;
    println!("stop far {trigger} x {held}: {placed:?}");
    let working = exchange.working_orders(&btc).await.unwrap();
    println!(
        "openOrders has it: {}",
        working
            .iter()
            .any(|order| order.client_order_id.as_ref() == Some(&far))
    );
    println!("status: {:?}", exchange.order_status(&btc, &far).await);

    // 2. 撤掉。
    println!("cancel: {:?}", exchange.cancel_order(&btc, &far).await);
    println!(
        "status after cancel: {:?}",
        exchange.order_status(&btc, &far).await
    );
    println!(
        "cancel again: {:?}",
        exchange.cancel_order(&btc, &far).await
    );

    // 3. 不足最低金额的止损单（只减一部分）。
    let tiny = Quantity::parse("0.00002").unwrap();
    let small = new_id();
    println!(
        "stop tiny x {tiny}: {:?}",
        exchange
            .place_stop(&StopRequest {
                instrument: btc.clone(),
                client_order_id: small.clone(),
                side: Side::Sell,
                trigger_price: trigger,
                limit_price: price(as_f64(trigger) * 0.9),
                quantity: tiny,
                expires_at: Utc::now() + Duration::seconds(10),
            })
            .await
    );
    let _ = exchange.cancel_order(&btc, &small).await;

    // 3b. 数量大于仓位的止损单。
    let over = new_id();
    println!(
        "stop over x {}: {:?}",
        held.saturating_add(held),
        exchange
            .place_stop(&StopRequest {
                instrument: btc.clone(),
                client_order_id: over.clone(),
                side: Side::Sell,
                trigger_price: trigger,
                limit_price: price(as_f64(trigger) * 0.9),
                quantity: held.saturating_add(held),
                expires_at: Utc::now() + Duration::seconds(10),
            })
            .await
    );
    println!(
        "status over: {:?}",
        exchange.order_status(&btc, &over).await
    );
    let _ = exchange.cancel_order(&btc, &over).await;

    // 4. 触发价已经在标记价之上：看会不会立即触发。
    let mark = exchange.mark_price(&btc).await.unwrap();
    let hot = new_id();
    let trigger = price(as_f64(mark) * 1.01);
    let started = Utc::now();
    println!(
        "stop hot {trigger} (mark {mark}): {:?}",
        exchange
            .place_stop(&StopRequest {
                instrument: btc.clone(),
                client_order_id: hot.clone(),
                side: Side::Sell,
                trigger_price: trigger,
                limit_price: price(as_f64(trigger) * 0.9),
                quantity: held,
                expires_at: Utc::now() + Duration::seconds(10),
            })
            .await
    );
    for _ in 0..5 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let status = exchange.order_status(&btc, &hot).await;
        println!("status hot: {status:?}");
        if let Ok(OrderLookup::Found(order)) = &status
            && !order.cumulative_filled.is_zero()
        {
            break;
        }
    }
    let fills = exchange
        .fills_since(&btc, started - Duration::seconds(5))
        .await
        .unwrap();
    for fill in fills.iter().filter(|fill| fill.client_order_id == hot) {
        println!(
            "hot fill {} @ {} fee {:?}",
            fill.quantity, fill.price, fill.fee
        );
    }
    let _ = exchange.cancel_order(&btc, &hot).await;

    // 5. 平掉这次多出来的仓位。
    let position = exchange.position(&btc).await.unwrap();
    let extra = position.units() - before.units();
    if extra > 0 {
        let mark = exchange.mark_price(&btc).await.unwrap();
        let closed = exchange
            .place_order(&OrderRequest {
                instrument: btc.clone(),
                client_order_id: new_id(),
                side: Side::Sell,
                limit_price: price(as_f64(mark) * 0.99),
                quantity: Quantity::from_units(extra),
                reduce_only: true,
                time_in_force: TimeInForce::ImmediateOrCancel,
                expires_at: Utc::now() + Duration::seconds(10),
            })
            .await;
        println!("close {}: {closed:?}", Quantity::from_units(extra));
    }
    let after: Position = exchange.position(&btc).await.unwrap();
    println!("position after {after} (before {before})");
}
