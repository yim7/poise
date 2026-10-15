//! 做多网格在模拟交易所上的完整循环：挂买、成交、挂卖、卖出、重新挂买回。

mod common;

use common::{at, harness};
use poise_strategy::order::{OrderPurpose, OrderStatus};
use poise_strategy::units::{Position, Price, Quantity};

#[tokio::test]
async fn b24_every_cell_is_its_own_unit_and_only_buys_what_it_holds() {
    let harness = harness();

    let report = harness.step(0).await;

    assert_eq!(report.placed.len(), 20);
    assert_eq!(report.applied_fills, 0);
    assert_eq!(report.position, Position::ZERO);
    assert_eq!(report.expected_position, Position::ZERO);

    // 没有主仓时每格自成单位：各自在自己的买价挂一份买单，没有卖单。
    for cell in 0..20 {
        let cloid = harness.working_cloid(cell, OrderPurpose::GridBuy);
        let order = harness.at_exchange(&cloid).unwrap();
        assert_eq!(
            order.limit_price,
            Price::parse(&(95000 - cell * 1000).to_string()).unwrap()
        );
        assert_eq!(order.quantity, Quantity::parse("0.05").unwrap());
        assert!(!order.reduce_only);
    }
    assert_eq!(harness.working().len(), 20);
    assert!(
        harness
            .working()
            .iter()
            .all(|order| order.record.purpose == OrderPurpose::GridBuy)
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.05").unwrap()
    );
}

#[tokio::test]
async fn b07_partially_filled_cell_keeps_a_buy_back_and_a_sell_that_do_not_match() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridBuy);

    // 95 格买回 0.05，只成交 0.02。
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.02").unwrap(), at(1))
        .unwrap();
    let report = harness.step(1).await;

    assert_eq!(report.applied_fills, 1);
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    // 待买回从 0.05 降到 0.03。
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );

    // 同一格同时挂着剩余 0.03 的买回单和 0.02 的卖单，价格不同，不会互相成交。
    let buy_back = harness.at_exchange(&buy).unwrap();
    assert_eq!(buy_back.status, OrderStatus::PartiallyFilled);
    assert_eq!(buy_back.cumulative_filled, Quantity::parse("0.02").unwrap());
    let sell = harness.working_cloid(0, OrderPurpose::GridSell);
    let sell_order = harness.at_exchange(&sell).unwrap();
    assert_eq!(sell_order.limit_price, Price::parse("96000").unwrap());
    assert_eq!(sell_order.quantity, Quantity::parse("0.02").unwrap());
    assert!(sell_order.reduce_only);
    assert_ne!(buy_back.limit_price, sell_order.limit_price);

    // 其余格子只挂着各自的买单。
    assert_eq!(harness.working().len(), 21);
}

#[tokio::test]
async fn b09_after_an_upward_breakout_every_cell_is_sold_and_buys_back_at_its_own_price() {
    let harness = harness();
    harness.step(0).await;

    // 价格跌到网格下方：20 格全部按各自的买价成交。
    harness
        .exchange
        .set_market_price(Price::parse("75000").unwrap(), at(1));
    let bought = harness.step(1).await;
    assert_eq!(bought.applied_fills, 20);
    assert_eq!(bought.position, Position::parse("1").unwrap());
    assert_eq!(bought.expected_position, Position::parse("1").unwrap());
    assert_eq!(bought.placed.len(), 20);

    // 向上突破：每格按各自的卖价卖出，并重新挂上各自的买回单。
    harness
        .exchange
        .set_market_price(Price::parse("96100").unwrap(), at(2));
    let sold = harness.step(2).await;
    assert_eq!(sold.applied_fills, 20);
    assert_eq!(sold.position, Position::ZERO);
    assert_eq!(sold.expected_position, Position::ZERO);
    assert_eq!(sold.placed.len(), 20);
    assert!(
        harness
            .orders()
            .iter()
            .filter(|order| order.record.purpose == OrderPurpose::GridSell)
            .all(|order| !order.record.is_working()),
        "every cell was sold"
    );

    // 买回单留在各自的买价，数量回到整份。
    for cell in 0..20 {
        let cloid = harness.working_cloid(cell, OrderPurpose::GridBuy);
        let order = harness.at_exchange(&cloid).unwrap();
        assert_eq!(
            order.limit_price,
            Price::parse(&(95000 - cell * 1000).to_string()).unwrap()
        );
        assert_eq!(order.quantity, Quantity::parse("0.05").unwrap());
    }
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.05").unwrap()
    );
    assert_eq!(harness.working().len(), 20);

    // 再走一步不会重复挂单。
    let idle = harness.step(3).await;
    assert!(idle.placed.is_empty());
    assert!(idle.canceled.is_empty());
    assert_eq!(harness.working().len(), 20);
}
