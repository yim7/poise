//! 适配层测试：全部离线，用脱敏样本当交易所的回复。

use chrono::{DateTime, TimeZone, Utc};
use poise_runtime::{
    Account, AccountMode, Exchange, ExchangeError, Network, OrderLookup, OrderRequest, SecretKey,
    TimeInForce,
};
use poise_strategy::instrument::{Fees, Instrument, PriceRule};
use poise_strategy::order::{ClientOrderId, OrderStatus, Side};
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};
use serde_json::{Value, json};

use crate::actions::{ExchangeAction, LimitOrderType, OrderAction, OrderType};
use crate::fixtures;
use crate::signing::{action_hash, sign_l1_action};
use crate::testing::{ACCOUNT, API_WALLET, FakeTransport, PRIVATE_KEY, client, fixed_clock};

const NOW_MS: u64 = 1_792_000_000_000;
const CLOID: &str = "0x11111111111111111111111111111111";

fn at(millis: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis).unwrap()
}

fn btc() -> Instrument {
    Instrument::try_new("BTC").unwrap()
}

fn cloid(value: &str) -> ClientOrderId {
    ClientOrderId::try_new(value).unwrap()
}

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn request(quantity: &str, price: &str) -> OrderRequest {
    OrderRequest {
        instrument: btc(),
        client_order_id: cloid(CLOID),
        side: Side::Buy,
        limit_price: Price::parse(price).unwrap(),
        quantity: Quantity::parse(quantity).unwrap(),
        reduce_only: false,
        time_in_force: TimeInForce::GoodTillCanceled,
        expires_at: at(NOW_MS as i64 + 5_000),
    }
}

/// 一页成交，用来构造分页。
fn fill(tid: u64, time: i64, cloid: Option<&str>, size: &str) -> Value {
    let mut value = json!({
        "coin": "BTC",
        "px": "95000.0",
        "sz": size,
        "side": "B",
        "time": time,
        "oid": 1000 + tid,
        "tid": tid,
    });
    if let Some(cloid) = cloid {
        value["cloid"] = json!(cloid);
    }
    value
}

// ── 下单 ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn place_order_signs_and_sends_the_order_then_maps_a_resting_status() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::META),
        fixture(fixtures::ORDER_RESTING),
    ]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let receipt = client
        .place_order(&request("0.001", "94000"))
        .await
        .unwrap();

    assert_eq!(receipt.exchange_order_id, "1004");
    assert_eq!(receipt.cumulative_filled, Quantity::ZERO);
    assert_eq!(receipt.status, OrderStatus::Open);

    assert_eq!(transport.info_types(), vec!["meta"]);
    let sent = transport.request(1);
    assert_eq!(sent.path, "/exchange");
    assert_eq!(sent.body["nonce"], NOW_MS);
    // 每张单都带交易所侧的有效期（设计 §9.1），到点未成交由交易所自己撤。
    let expires_after = NOW_MS + 5_000;
    assert_eq!(sent.body["expiresAfter"], expires_after);
    assert_eq!(sent.body["action"]["type"], "order");
    let order = &sent.body["action"]["orders"][0];
    assert_eq!(order["a"], 1, "BTC 在样本里是第 2 个资产");
    assert_eq!(order["b"], true);
    assert_eq!(order["p"], "94000");
    assert_eq!(order["s"], "0.001");
    assert_eq!(order["r"], false);
    assert_eq!(order["t"], json!({"limit": {"tif": "Gtc"}}));
    assert_eq!(order["c"], CLOID);
    assert!(matches!(
        sent.body["signature"]["v"].as_u64(),
        Some(27 | 28)
    ));
    assert!(
        sent.body["signature"]["r"]
            .as_str()
            .unwrap()
            .starts_with("0x")
    );

    // 交易所会拿请求体自己重算动作哈希，所以这里也重算一遍：发出去的签名必须
    // 正好覆盖带有效期的那份哈希，而不是不带有效期的那份。
    let action = sent_order_action("94000", "0.001");
    assert_eq!(
        sent.body["signature"],
        signature_of(&action, NOW_MS, Some(expires_after))
    );
    assert_ne!(
        sent.body["signature"],
        signature_of(&action, NOW_MS, None),
        "有效期必须进哈希，否则交易所算出来的哈希与我们签的不是同一个"
    );
}

/// 上面那个测试里发出去的动作，用来重算签名。
fn sent_order_action(price: &str, quantity: &str) -> ExchangeAction {
    ExchangeAction::Order(OrderAction {
        orders: vec![crate::actions::OrderRequest {
            asset: 1,
            is_buy: true,
            limit_px: price.to_string(),
            sz: quantity.to_string(),
            reduce_only: false,
            order_type: OrderType::Limit(LimitOrderType { tif: "Gtc" }),
            cloid: CLOID.to_string(),
        }],
        grouping: "na",
    })
}

/// 按测试私钥重算一份签名，模拟交易所那边独立算一次。
fn signature_of(action: &ExchangeAction, nonce: u64, expires_after: Option<u64>) -> Value {
    let key = SecretKey::parse(PRIVATE_KEY).unwrap();
    let hash = action_hash(action, nonce, expires_after).unwrap();
    serde_json::to_value(sign_l1_action(&key, Network::Testnet, hash).unwrap()).unwrap()
}

#[tokio::test]
async fn place_order_reports_an_immediate_fill_with_its_size() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::META),
        fixture(fixtures::ORDER_FILLED),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    let receipt = client
        .place_order(&request("0.001", "96000"))
        .await
        .unwrap();

    assert_eq!(receipt.status, OrderStatus::Filled);
    assert_eq!(receipt.cumulative_filled, Quantity::parse("0.001").unwrap());
}

/// 观点失效的减仓单（第 7.5 节）：按 `Ioc` 发出；只成交了一部分时剩下的已经撤掉，记成
/// 已撤销，不能当成全部成交。
#[tokio::test]
async fn an_immediate_or_cancel_order_is_sent_as_ioc_and_a_partial_fill_is_canceled() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::META),
        fixture(fixtures::ORDER_FILLED),
    ]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));
    let request = OrderRequest {
        reduce_only: true,
        time_in_force: TimeInForce::ImmediateOrCancel,
        side: Side::Sell,
        ..request("0.002", "94000")
    };

    let receipt = client.place_order(&request).await.unwrap();

    let order = &transport.request(1).body["action"]["orders"][0];
    assert_eq!(order["t"], json!({"limit": {"tif": "Ioc"}}));
    assert_eq!(order["r"], true);
    assert_eq!(receipt.cumulative_filled, Quantity::parse("0.001").unwrap());
    assert_eq!(receipt.status, OrderStatus::Canceled);
}

#[tokio::test]
async fn place_order_reports_a_rejection_with_the_exchange_reason() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::META),
        fixture(fixtures::ORDER_REJECTED),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    let error = client
        .place_order(&request("0.001", "94000"))
        .await
        .unwrap_err();

    assert!(
        matches!(&error, ExchangeError::InsufficientMargin { reason } if reason.contains("Insufficient margin")),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn place_order_reports_a_rejected_action() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::META),
        fixture(fixtures::ACTION_REJECTED),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    let error = client
        .place_order(&request("0.001", "94000"))
        .await
        .unwrap_err();

    assert!(
        matches!(&error, ExchangeError::Rejected { reason } if reason.contains("Must deposit")),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn place_order_treats_a_transport_failure_as_an_unknown_result() {
    let transport = FakeTransport::scripted(vec![
        Ok(fixture(fixtures::META)),
        Err(ExchangeError::Unknown {
            reason: "the exchange did not respond".to_string(),
        }),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    let error = client
        .place_order(&request("0.001", "94000"))
        .await
        .unwrap_err();

    assert!(
        matches!(error, ExchangeError::Unknown { .. }),
        "结果未知时不能当成没下单，否则会重复下单：{error}"
    );
}

#[tokio::test]
async fn place_order_refuses_prices_and_sizes_the_exchange_would_refuse() {
    let transport = FakeTransport::new(vec![fixture(fixtures::META)]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    // 6 位有效数字。
    let error = client
        .place_order(&request("0.001", "94000.5"))
        .await
        .unwrap_err();
    assert!(matches!(error, ExchangeError::Rejected { .. }), "{error}");

    // 数量精度是 0.00001。
    let error = client
        .place_order(&request("0.001234", "94000"))
        .await
        .unwrap_err();
    assert!(matches!(error, ExchangeError::Rejected { .. }), "{error}");

    let sent = transport.requests();
    assert_eq!(sent.len(), 1, "本地就该拒绝的订单不发给交易所");
    assert_eq!(sent[0].path, "/info");
}

#[tokio::test]
async fn place_order_refuses_a_client_order_id_hyperliquid_cannot_use() {
    let transport = FakeTransport::new(vec![fixture(fixtures::META)]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));
    let mut request = request("0.001", "94000");
    request.client_order_id = cloid("order-1");

    let error = client.place_order(&request).await.unwrap_err();

    assert!(
        matches!(&error, ExchangeError::Rejected { reason } if reason.contains("0x")),
        "unexpected error: {error}"
    );
    assert_eq!(transport.requests().len(), 1);
}

// ── 撤单 ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_order_sends_the_cloid_and_reports_success() {
    let transport = FakeTransport::new(vec![fixture(fixtures::META), fixture(fixtures::CANCEL_OK)]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    client.cancel_order(&btc(), &cloid(CLOID)).await.unwrap();

    let sent = transport.request(1);
    assert_eq!(sent.body["action"]["type"], "cancelByCloid");
    assert_eq!(
        sent.body["action"]["cancels"][0],
        json!({"asset": 1, "cloid": CLOID})
    );
    assert!(
        sent.body.get("expiresAfter").is_none(),
        "撤单是立刻生效的，官方 SDK 的 cancelByCloid 也不带有效期"
    );
}

#[tokio::test]
async fn cancel_order_of_an_order_that_is_already_gone_is_not_a_rejection() {
    for response in [
        fixtures::CANCEL_ALREADY_DONE,
        fixtures::CANCEL_ALREADY_DONE_OLD_WORDING,
    ] {
        let transport = FakeTransport::new(vec![fixture(fixtures::META), fixture(response)]);
        let client = client(transport, fixed_clock(NOW_MS));

        let error = client
            .cancel_order(&btc(), &cloid(CLOID))
            .await
            .unwrap_err();

        // 报「拒绝」会让调用方按「这单没了」处理；这张单可能已经成交，必须说「不认识」。
        assert!(
            matches!(error, ExchangeError::UnknownOrder { .. }),
            "unexpected error for {response}: {error}"
        );
    }
}

// ── 挂单与订单状态 ────────────────────────────────────────────────────────────

#[tokio::test]
async fn working_orders_list_every_order_on_this_instrument() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::OPEN_ORDERS_OURS),
        fixture(fixtures::OPEN_ORDERS),
    ]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let ours = client.working_orders(&btc()).await.unwrap();
    let foreign = client.working_orders(&btc()).await.unwrap();

    assert_eq!(ours.len(), 1);
    let order = &ours[0];
    assert_eq!(order.client_order_id, Some(cloid(CLOID)));
    assert_eq!(order.exchange_order_id, "1004");
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.limit_price, Price::parse("94000").unwrap());
    assert_eq!(order.quantity, Quantity::parse("0.001").unwrap());
    assert_eq!(
        order.cumulative_filled,
        Quantity::parse("0.0004").unwrap(),
        "累计成交量 = 原始数量 − 剩余数量"
    );
    assert_eq!(order.status, OrderStatus::Open);
    // 没有 cloid 的挂单不是本系统下的：照样列出（预览要给用户看），对账时按 cloid 自然忽略。
    assert_eq!(foreign.len(), 1);
    assert_eq!(foreign[0].client_order_id, None);

    let sent = transport.request(0);
    assert_eq!(sent.body, json!({"type": "openOrders", "user": ACCOUNT}));
}

#[tokio::test]
async fn order_status_of_an_unknown_order_is_unknown_not_canceled() {
    let transport = FakeTransport::new(vec![fixture(fixtures::ORDER_STATUS_UNKNOWN)]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let lookup = client.order_status(&btc(), &cloid(CLOID)).await.unwrap();

    assert_eq!(lookup, OrderLookup::Unknown);
    assert_eq!(
        transport.request(0).body,
        json!({"type": "orderStatus", "user": ACCOUNT, "oid": CLOID})
    );
}

#[tokio::test]
async fn order_status_reports_the_current_state_of_a_known_order() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::ORDER_STATUS_OPEN),
        fixture(fixtures::ORDER_STATUS_CANCELED),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    let open = client.order_status(&btc(), &cloid(CLOID)).await.unwrap();
    let canceled = client.order_status(&btc(), &cloid(CLOID)).await.unwrap();

    let OrderLookup::Found(open) = open else {
        panic!("expected a found order");
    };
    assert_eq!(open.status, OrderStatus::Open);
    assert_eq!(open.client_order_id, Some(cloid(CLOID)));
    assert_eq!(open.limit_price, Price::parse("94000").unwrap());
    let OrderLookup::Found(canceled) = canceled else {
        panic!("expected a found order");
    };
    assert_eq!(canceled.status, OrderStatus::Canceled);
}

/// 一张 `orderStatus` 回复：原始数量 0.001，剩余 `remaining`。
fn order_status(status: &str, remaining: &str) -> Value {
    json!({
        "status": "order",
        "order": {
            "order": {
                "coin": "BTC",
                "side": "B",
                "limitPx": "94000.0",
                "sz": remaining,
                "oid": 1004,
                "origSz": "0.001",
                "cloid": CLOID,
            },
            "status": status,
            "statusTimestamp": NOW_MS,
        }
    })
}

/// 撤销的取值不止 `canceled` 一个，但它们都是「订单结束了」，并且要带上最终的累计成交量。
#[tokio::test]
async fn every_cancel_status_ends_the_order_and_keeps_the_final_fill() {
    for status in [
        "canceled",
        "marginCanceled",
        "selfTradeCanceled",
        "reduceOnlyCanceled",
        "openInterestCapCanceled",
        "siblingFilledCanceled",
        "delistedCanceled",
        "liquidatedCanceled",
        "scheduledCancel",
        "vaultWithdrawalCanceled",
    ] {
        let transport = FakeTransport::new(vec![order_status(status, "0.0006")]);
        let client = client(transport, fixed_clock(NOW_MS));

        let lookup = client.order_status(&btc(), &cloid(CLOID)).await.unwrap();

        let OrderLookup::Found(order) = lookup else {
            panic!("expected a found order for {status}");
        };
        assert_eq!(order.status, OrderStatus::Canceled, "{status}");
        assert!(!order.status.is_working(), "{status} 是终态");
        assert_eq!(
            order.cumulative_filled,
            Quantity::parse("0.0004").unwrap(),
            "{status} 要带上撤销前的成交量"
        );
    }
}

/// 拒绝也是终态，但没有任何成交。带原因的变体（设计文档里的 `minTradeNtlRejected`）同样算拒绝。
#[tokio::test]
async fn the_rejected_statuses_end_the_order() {
    for status in [
        "rejected",
        "minTradeNtlRejected",
        "marginRejected",
        "selfTradeRejected",
    ] {
        let transport = FakeTransport::new(vec![order_status(status, "0.001")]);
        let client = client(transport, fixed_clock(NOW_MS));

        let lookup = client.order_status(&btc(), &cloid(CLOID)).await.unwrap();

        let OrderLookup::Found(order) = lookup else {
            panic!("expected a found order for {status}");
        };
        assert_eq!(order.status, OrderStatus::Rejected, "{status}");
        assert_eq!(order.cumulative_filled, Quantity::ZERO, "{status}");
    }
}

/// 看不懂的状态仍然是「结果未知」，不会被当成已经结束。
#[tokio::test]
async fn an_unrecognized_status_stays_unknown_and_working() {
    let transport = FakeTransport::new(vec![order_status("somethingNewCanceled", "0.0006")]);
    let client = client(transport, fixed_clock(NOW_MS));

    let lookup = client.order_status(&btc(), &cloid(CLOID)).await.unwrap();

    let OrderLookup::Found(order) = lookup else {
        panic!("expected a found order");
    };
    assert_eq!(order.status, OrderStatus::Unknown);
    assert!(order.status.is_working(), "结果未知的订单仍然算在场");
}

// ── 成交 ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn fills_are_deduplicated_and_carry_a_per_order_running_total() {
    let transport = FakeTransport::new(vec![fixture(fixtures::USER_FILLS_BY_TIME)]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let fills = client
        .fills_since(&btc(), at(NOW_MS as i64 - 60_000))
        .await
        .unwrap();

    assert_eq!(fills.len(), 2, "没有 cloid 的成交不属于本策略");
    assert_eq!(fills[0].trade_id, "5001");
    assert_eq!(fills[0].client_order_id, cloid(CLOID));
    assert_eq!(fills[0].price, Price::parse("95000").unwrap());
    assert_eq!(fills[0].quantity, Quantity::parse("0.001").unwrap());
    assert_eq!(
        fills[0].cumulative_quantity,
        Quantity::parse("0.001").unwrap()
    );
    assert_eq!(fills[0].filled_at, at(1_792_000_000_000));
    assert_eq!(fills[1].trade_id, "5002");
    assert_eq!(
        fills[1].client_order_id,
        cloid("0x22222222222222222222222222222222")
    );
    assert_eq!(
        fills[1].cumulative_quantity,
        Quantity::parse("0.002").unwrap(),
        "另一张订单的累计量从它自己算起"
    );

    assert_eq!(
        transport.request(0).body,
        json!({
            "type": "userFillsByTime",
            "user": ACCOUNT,
            "startTime": NOW_MS - 60_000,
            "aggregateByTime": false,
        })
    );
}

#[tokio::test]
async fn a_partially_filled_order_accumulates_across_pages() {
    let mut first: Vec<Value> = (1..2_000)
        .map(|tid| fill(tid, 1_792_000_000_000, Some(CLOID), "0.001"))
        .collect();
    first.push(fill(2_000, 1_792_000_000_500, Some(CLOID), "0.001"));
    let second = vec![
        // 边界上的那一条会重复出现，按 tid 去重。
        fill(2_000, 1_792_000_000_500, Some(CLOID), "0.001"),
        fill(2_001, 1_792_000_000_500, Some(CLOID), "0.001"),
    ];
    let transport = FakeTransport::new(vec![
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap(),
    ]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let fills = client
        .fills_since(&btc(), at(1_792_000_000_000))
        .await
        .unwrap();

    assert_eq!(fills.len(), 2_001);
    assert_eq!(
        fills.last().unwrap().cumulative_quantity,
        Quantity::parse("2.001").unwrap(),
        "满页之后要继续翻页，累计量才是完整的"
    );
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body["startTime"], 1_792_000_000_500_i64,
        "从上一页最后一条的时间继续，边界那条靠 tid 去重"
    );
}

/// 被跳过的成交不能算进后面几笔的累计量：运行时拿这个累计量对账。
#[tokio::test]
async fn a_fill_that_is_dropped_does_not_inflate_later_running_totals() {
    let transport = FakeTransport::new(vec![
        serde_json::to_value(vec![
            // 时间戳超出可表示范围（排在最前），这笔会被丢掉。
            fill(5_001, i64::MIN, Some(CLOID), "0.001"),
            fill(5_002, 1_792_000_000_000, Some(CLOID), "0.002"),
        ])
        .unwrap(),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    let fills = client
        .fills_since(&btc(), at(1_792_000_000_000))
        .await
        .unwrap();

    assert_eq!(fills.len(), 1, "时间戳不可用的成交不进结果");
    assert_eq!(
        fills[0].cumulative_quantity,
        Quantity::parse("0.002").unwrap(),
        "被丢掉的成交不能算进累计量"
    );
}

/// 交易所只留最近 10000 笔成交，而且是整个账户的，不只这个标的。
#[tokio::test]
async fn a_window_that_reaches_the_exchange_retention_limit_is_refused() {
    let page = |offset: u64, millis: i64| {
        serde_json::to_value(
            (0..2_000)
                .map(|tid| {
                    // 别的标的的成交，一笔都不属于本策略。
                    let mut value = fill(offset + tid, millis + tid as i64, Some(CLOID), "0.001");
                    value["coin"] = json!("ETH");
                    value
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
    };
    let transport = FakeTransport::new(vec![
        page(0, 1_792_000_000_000),
        page(2_000, 1_792_000_002_000),
        page(4_000, 1_792_000_004_000),
        page(6_000, 1_792_000_006_000),
        page(8_000, 1_792_000_008_000),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    let error = client
        .fills_since(&btc(), at(1_792_000_000_000))
        .await
        .unwrap_err();

    assert!(
        matches!(&error, ExchangeError::Unknown { reason } if reason.contains("retention limit")),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn two_different_fills_with_the_same_trade_id_stop_the_window() {
    let transport = FakeTransport::new(vec![
        serde_json::to_value(vec![
            fill(5_001, 1_792_000_000_000, Some(CLOID), "0.001"),
            fill(5_001, 1_792_000_000_000, Some(CLOID), "0.002"),
        ])
        .unwrap(),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    let error = client
        .fills_since(&btc(), at(1_792_000_000_000))
        .await
        .unwrap_err();

    assert!(
        matches!(&error, ExchangeError::Unknown { reason } if reason.contains("trade id")),
        "unexpected error: {error}"
    );
}

// ── 仓位与标记价 ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn position_comes_from_the_clearinghouse_state() {
    let transport = FakeTransport::new(vec![fixture(fixtures::CLEARINGHOUSE_STATE)]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let position = client.position(&btc()).await.unwrap();

    assert_eq!(position, Position::parse("0.005").unwrap());
    assert_eq!(
        transport.request(0).body,
        json!({"type": "clearinghouseState", "user": ACCOUNT})
    );
}

#[tokio::test]
async fn position_of_an_instrument_without_a_position_is_zero() {
    let transport = FakeTransport::new(vec![fixture(fixtures::CLEARINGHOUSE_STATE)]);
    let client = client(transport, fixed_clock(NOW_MS));

    let position = client
        .position(&Instrument::try_new("SOL").unwrap())
        .await
        .unwrap();

    assert_eq!(position, Position::ZERO);
}

#[tokio::test]
async fn mark_price_comes_from_the_asset_contexts() {
    let transport = FakeTransport::new(vec![fixture(fixtures::META_AND_ASSET_CTXS)]);
    let client = client(transport, fixed_clock(NOW_MS));

    assert_eq!(
        client.mark_price(&btc()).await.unwrap(),
        Price::parse("95000").unwrap()
    );
}

// ── 账户 ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn account_mode_is_read_from_user_abstraction() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::USER_ABSTRACTION),
        json!("default"),
        Value::Null,
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    assert_eq!(client.mode().await.unwrap(), AccountMode::Unified);
    assert_eq!(
        client.mode().await.unwrap(),
        AccountMode::Other("default".to_string())
    );
    assert_eq!(
        client.mode().await.unwrap(),
        AccountMode::Other("<unknown>".to_string()),
        "读不出来的模式不能当成 Unified"
    );
}

#[tokio::test]
async fn authorized_api_wallets_come_from_extra_agents() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::EXTRA_AGENTS),
        // 交易所可能在数组里放 `null`。
        json!([null, {"name": "poise", "address": API_WALLET, "validUntil": 1_800_000_000_000_i64}]),
    ]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let wallets = client.authorized_api_wallets().await.unwrap();
    let with_null = client.authorized_api_wallets().await.unwrap();

    assert_eq!(wallets.len(), 1);
    assert_eq!(wallets[0].name, "poise");
    assert_eq!(wallets[0].valid_until, at(1_800_000_000_000));
    assert_eq!(with_null.len(), 1);
    assert_eq!(with_null[0].address.to_string(), API_WALLET);
    assert_eq!(
        transport.request(0).body,
        json!({"type": "extraAgents", "user": ACCOUNT})
    );
}

#[tokio::test]
async fn available_collateral_uses_the_amount_left_after_maintenance() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::SPOT_CLEARINGHOUSE_STATE),
        // 没有这一项时退回「总额 − 冻结」。
        json!({"balances": [{"coin": "USDC", "token": 0, "total": "1000.0", "hold": "120.5"}]}),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    assert_eq!(
        client.available_collateral().await.unwrap(),
        Notional::parse("870.25").unwrap()
    );
    assert_eq!(
        client.available_collateral().await.unwrap(),
        Notional::parse("879.5").unwrap()
    );
}

/// 账户比率的组成：现货 USDC 余额、全仓维持保证金占用、逐仓仓位的保证金。
#[tokio::test]
async fn the_margin_reading_combines_spot_and_perp_state() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::SPOT_CLEARINGHOUSE_STATE),
        json!({
            "crossMaintenanceMarginUsed": "12.5",
            "time": 1_792_000_000_000_i64,
            "assetPositions": [
                {"type": "oneWay", "position": {"coin": "BTC", "szi": "0.005",
                    "leverage": {"type": "cross", "value": 10}, "marginUsed": "50.0"}},
                {"type": "oneWay", "position": {"coin": "ETH", "szi": "-0.1",
                    "leverage": {"type": "isolated", "value": 5, "rawUsd": "100"}, "marginUsed": "40.0"}}
            ]
        }),
    ]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let margin = client.margin().await.unwrap();

    assert_eq!(margin.balance, Notional::parse("1000").unwrap());
    assert_eq!(
        margin.available_after_maintenance,
        Notional::parse("870.25").unwrap()
    );
    assert_eq!(margin.cross_maintenance, Notional::parse("12.5").unwrap());
    assert_eq!(margin.isolated_margin, Notional::parse("40").unwrap());
    assert_eq!(
        margin.as_of,
        DateTime::from_timestamp_millis(1_792_000_000_000)
    );
    // 整个账户的占用 1000 − 870.25 = 129.75 比默认 DEX 的 12.5 大：129.75 ÷ (1000 − 40)。
    assert_eq!(margin.ratio(), Rate::parse("0.13515625").unwrap());
    assert_eq!(
        transport.request(1).body,
        json!({"type": "clearinghouseState", "user": ACCOUNT})
    );
}

/// 仓位没说是全仓还是逐仓：不知道要不要扣它的保证金，报「读不到」。
#[tokio::test]
async fn a_position_without_a_margin_mode_is_unreadable() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::SPOT_CLEARINGHOUSE_STATE),
        json!({
            "crossMaintenanceMarginUsed": "12.5",
            "time": 1_792_000_000_000_i64,
            "assetPositions": [{"type": "oneWay", "position": {"coin": "ETH", "szi": "-0.1", "marginUsed": "40.0"}}]
        }),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    assert!(matches!(
        client.margin().await,
        Err(ExchangeError::Unavailable { .. })
    ));
}

/// 交易所没给读数时刻：判断不了有没有过期，报「读不到」。
#[tokio::test]
async fn a_margin_reading_without_a_time_is_unreadable() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::SPOT_CLEARINGHOUSE_STATE),
        json!({"crossMaintenanceMarginUsed": "12.5", "assetPositions": []}),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    assert!(matches!(
        client.margin().await,
        Err(ExchangeError::Unavailable { .. })
    ));
}

/// 交易所没给全仓维持保证金时读不出账户比率：报「读不到」，闸门关上，而不是当成 0。
#[tokio::test]
async fn a_missing_cross_maintenance_margin_is_unreadable() {
    let transport = FakeTransport::new(vec![
        fixture(fixtures::SPOT_CLEARINGHOUSE_STATE),
        json!({"assetPositions": []}),
    ]);
    let client = client(transport, fixed_clock(NOW_MS));

    assert!(matches!(
        client.margin().await,
        Err(ExchangeError::Unavailable { .. })
    ));
}

/// 杠杆来自 `activeAssetData`（测试网实测的返回格式）。
#[tokio::test]
async fn the_leverage_comes_from_active_asset_data() {
    let transport = FakeTransport::new(vec![json!({
        "user": ACCOUNT,
        "coin": "BTC",
        "leverage": {"type": "cross", "value": 20},
        "maxTradeSzs": ["0.24452", "0.24452"],
        "availableToTrade": ["998.903866", "998.903866"],
        "markPx": "81703.0"
    })]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    assert_eq!(
        client.leverage(&btc()).await.unwrap(),
        poise_runtime::account::Leverage {
            value: 20,
            cross: true
        }
    );
    assert_eq!(
        transport.request(0).body,
        json!({"type": "activeAssetData", "user": ACCOUNT, "coin": "BTC"})
    );
}

/// 保证金分档表从 `meta` 里按标的的表编号找；找不到时按最大杠杆只有一档。
#[tokio::test]
async fn the_margin_tiers_come_from_the_meta() {
    let meta = json!({
        "universe": [
            {"name": "ETH", "szDecimals": 4, "maxLeverage": 25, "marginTableId": 25},
            {"name": "BTC", "szDecimals": 5, "maxLeverage": 40, "marginTableId": 54}
        ],
        "marginTables": [
            [54, {"description": "tiered 40x", "marginTiers": [
                {"lowerBound": "0.0", "maxLeverage": 40},
                {"lowerBound": "10000.0", "maxLeverage": 25}
            ]}]
        ]
    });
    let transport = FakeTransport::new(vec![meta.clone(), meta]);
    let client = client(transport, fixed_clock(NOW_MS));

    let btc_tiers = client.margin_tiers(&btc()).await.unwrap();
    // 75000 的名义价值：75000 × 2% − 10000 × (2% − 1.25%) = 1425。
    assert_eq!(
        btc_tiers.maintenance_margin(Notional::parse("75000").unwrap()),
        Notional::parse("1425").unwrap()
    );
    let eth_tiers = client
        .margin_tiers(&Instrument::try_new("ETH").unwrap())
        .await
        .unwrap();
    assert_eq!(eth_tiers, poise_strategy::stress::MarginTiers::single(25));
}

#[tokio::test]
async fn fees_come_from_user_fees() {
    let transport = FakeTransport::new(vec![fixture(fixtures::USER_FEES)]);
    let client = client(transport, fixed_clock(NOW_MS));

    assert_eq!(
        client.fees().await.unwrap(),
        Fees::new(
            Rate::parse("0.00045").unwrap(),
            Rate::parse("0.00015").unwrap()
        )
    );
}

#[tokio::test]
async fn instrument_spec_is_built_from_the_contract_rules() {
    // 只要 `meta`：报价规则随价格变化，不必再取标记价。
    let transport = FakeTransport::new(vec![fixture(fixtures::META)]);
    let client = client(transport.clone(), fixed_clock(NOW_MS));

    let spec = client.instrument_spec(&btc()).await.unwrap();

    assert_eq!(transport.info_types(), vec!["meta"]);
    assert_eq!(spec.instrument(), &btc());
    assert_eq!(
        spec.price_rule(),
        PriceRule::SignificantFigures {
            max_figures: 5,
            step_floor: Price::parse("0.1").unwrap(),
        }
    );
    assert_eq!(
        spec.price_rule().step_at(Price::parse("95000").unwrap()),
        Price::parse("1").unwrap()
    );
    assert_eq!(spec.quantity_step(), Quantity::parse("0.00001").unwrap());
    assert_eq!(spec.min_order_notional(), Notional::parse("10").unwrap());
}

#[tokio::test]
async fn an_instrument_the_exchange_does_not_list_is_refused() {
    let transport = FakeTransport::new(vec![fixture(fixtures::META)]);
    let client = client(transport, fixed_clock(NOW_MS));

    let error = client
        .instrument_spec(&Instrument::try_new("DOGE").unwrap())
        .await
        .unwrap_err();

    assert!(
        matches!(&error, ExchangeError::Unavailable { reason } if reason.contains("DOGE")),
        "unexpected error: {error}"
    );
}
