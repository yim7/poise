//! 首次接管已有仓位（第 8 节，B13b、B21、B22）。

mod common;

use std::sync::Arc;

use common::{Console, account, at, console_over, core_long_config, exchange};
use poise_runtime::{Exchange, ManualClock, OrderRequest, Store, StrategyStatus};
use poise_strategy::batch::BatchIndex;
use poise_strategy::order::{ClientOrderId, OrderPurpose, Side, Unit};
use poise_strategy::side::Direction;
use poise_strategy::units::{Position, Price, Quantity};

fn quantity(value: &str) -> Quantity {
    Quantity::parse(value).unwrap()
}

/// 标的上已有 `position`、行情在 `market` 的控制台。
fn console_with_position(position: &str, market: &str) -> Console {
    let exchange = exchange();
    exchange.set_position(Position::parse(position).unwrap());
    exchange.set_market_price(Price::parse(market).unwrap(), at(0));
    console_over(
        Store::open_in_memory().unwrap(),
        exchange,
        account(),
        Arc::new(ManualClock::new(at(0))),
    )
}

/// B21、B13b：已有 .37 多单接管设计第 3.2 节的做多网格，价格在 88（千美元）。
///
/// 预览给出第 8 节例子的分配；第一步挂出持有格子的卖单、88 格补回 .009，以及 90 批还差
/// 的主仓——它的限价从 90 下调到本策略最低卖价 89 下方一个报价单位，不与自己的卖单相交。
/// 接管后应有持仓等于实际仓位，仓位核对不会暂停策略。
#[tokio::test]
async fn b21_b13b_an_existing_long_position_is_taken_over() {
    let console = console_with_position("0.37", "88000");
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(preview.validation.valid, "{:?}", preview.validation.reason);
    assert_eq!(preview.position, Position::parse("0.37").unwrap());
    assert_eq!(preview.batches[0].core_filled, quantity("0.175"));
    assert!(preview.batches[0].finished);
    assert_eq!(preview.batches[1].core_filled, quantity("0.084"));
    assert!(!preview.batches[1].finished);
    let core_buy = preview
        .planned_orders
        .iter()
        .find(|intent| intent.unit == Unit::Batch(Direction::Long, BatchIndex(1)))
        .expect("the second batch keeps buying its core");
    assert_eq!(core_buy.quantity, quantity("0.091"));
    assert_eq!(
        core_buy.limit_price,
        Price::parse("88999").unwrap(),
        "限价下调到自身最低卖价 89 下方一个报价单位"
    );
    let exits: Vec<_> = preview
        .planned_orders
        .iter()
        .filter(|intent| intent.purpose == OrderPurpose::GridExit)
        .collect();
    assert_eq!(exits.len(), 8, "持有的八格各挂一张卖单");
    assert!(exits.iter().all(|intent| intent.side == Side::Sell));

    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console.clock.set(at(1));
    let report = console.runtime.step(&draft_id).await.unwrap();

    assert_eq!(report.status, StrategyStatus::Running);
    assert_eq!(report.position, report.expected_position);
    let placed_core = report
        .plan
        .place
        .iter()
        .find(|intent| intent.purpose == OrderPurpose::Core)
        .unwrap();
    assert_eq!(placed_core.limit_price, Price::parse("88999").unwrap());

    console.clock.set(at(2));
    let report = console.runtime.step(&draft_id).await.unwrap();
    assert_eq!(report.status, StrategyStatus::Running, "仓位核对对得上");
}

/// B22：已有仓位的方向没有对应的一侧时，预览不通过，确认启动无从谈起。
#[tokio::test]
async fn b22_a_short_position_cannot_be_taken_over_by_a_long_only_strategy() {
    let console = console_with_position("-0.1", "88000");
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(!preview.validation.valid);
    assert!(preview.token.is_none());
    let reason = preview.validation.reason.unwrap();
    assert!(reason.contains("no matching side"), "{reason}");
}

/// B22：已有仓位超过这一侧的上限时，预览不通过。
#[tokio::test]
async fn b22_a_position_above_the_max_position_cannot_be_taken_over() {
    let console = console_with_position("1.5", "88000");
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(!preview.validation.valid);
    let reason = preview.validation.reason.unwrap();
    assert!(reason.contains("max position"), "{reason}");
}

/// 交易所上已有的、非本系统下的挂单：预览里列出，启动、运行、停止都不撤它（第 8 节第 4 条）。
#[tokio::test]
async fn orders_that_are_not_ours_are_listed_and_left_alone() {
    let exchange = exchange();
    let foreign = ClientOrderId::try_new("0xforeign").unwrap();
    exchange
        .place_order(&OrderRequest {
            instrument: exchange.instrument().clone(),
            client_order_id: foreign.clone(),
            side: Side::Sell,
            limit_price: Price::parse("120000").unwrap(),
            quantity: quantity("0.01"),
            reduce_only: false,
            expires_at: at(10),
        })
        .await
        .unwrap();
    let console = console_over(
        Store::open_in_memory().unwrap(),
        exchange.clone(),
        account(),
        Arc::new(ManualClock::new(at(0))),
    );
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert_eq!(preview.foreign_orders.len(), 1);
    assert_eq!(
        preview.foreign_orders[0].client_order_id,
        Some(foreign.clone())
    );
    assert_eq!(preview.foreign_orders[0].side, Side::Sell);

    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console.clock.set(at(1));
    console.runtime.step(&draft_id).await.unwrap();
    console.clock.set(at(2));
    console.runtime.stop(&draft_id).await.unwrap();

    let still_there = exchange
        .orders()
        .into_iter()
        .find(|order| order.client_order_id.as_ref() == Some(&foreign))
        .unwrap();
    assert!(still_there.status.is_working(), "不撤销别人的挂单");
}
