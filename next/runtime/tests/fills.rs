//! 成交记账：重复、乱序、累计回退的成交只记一次（第 9.3 节）。

mod common;

use common::{at, harness};
use poise_strategy::order::{OrderPurpose, OrderStatus};
use poise_strategy::units::{Position, Price, Quantity};

#[tokio::test]
async fn b17_duplicate_out_of_order_and_regressed_fills_are_recorded_once() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);

    // 交易所先送来后发生的成交 0.01，累计量 0.02：先发生的那笔 0.01 还没到。
    // 订单累计量、待买回都跟着变。
    harness
        .exchange
        .inject_fill(
            "trade-1",
            &buy,
            Quantity::parse("0.01").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(1),
        )
        .unwrap();
    let first = harness.step(1).await;
    assert_eq!(first.applied_fills, 1);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );

    // 交易所重复投递同一笔成交（成交编号相同，这才是真实情况）、投递累计量回退的旧成交。
    harness
        .exchange
        .inject_fill(
            "trade-1",
            &buy,
            Quantity::parse("0.01").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(1),
        )
        .unwrap();
    harness
        .exchange
        .inject_fill(
            "trade-regressed",
            &buy,
            Quantity::parse("0.01").unwrap(),
            Quantity::parse("0.01").unwrap(),
            at(1),
        )
        .unwrap();
    // 模拟交易所原样返回投递进来的累计量：回退的成交不能先被它夹到已有累计量上，
    // 否则这里验证的只是重复值，回退这条路根本没被走到。
    let regressed = harness
        .exchange
        .fills()
        .into_iter()
        .find(|fill| fill.trade_id == "trade-regressed")
        .expect("the regressed fill must be visible");
    assert_eq!(
        regressed.cumulative_quantity,
        Quantity::parse("0.01").unwrap()
    );

    let second = harness.step(2).await;

    assert_eq!(second.applied_fills, 0);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
    // 流水照记，累计量原样保留：回退的成交是审计记录，不做修正。
    let recorded = harness.runtime.fills(&harness.strategy_id).unwrap();
    let regressed_row = recorded
        .iter()
        .find(|fill| fill.trade_id == "trade-regressed")
        .expect("the regressed fill must be recorded");
    assert_eq!(
        regressed_row.cumulative_quantity,
        Quantity::parse("0.01").unwrap()
    );
    let order = harness
        .orders()
        .into_iter()
        .find(|order| order.record.client_order_id == buy)
        .unwrap();
    assert_eq!(
        order.record.cumulative_filled,
        Quantity::parse("0.02").unwrap()
    );
    assert_eq!(order.record.status, OrderStatus::PartiallyFilled);
    // 记账结果与到达顺序无关：仓位仍然对得上，不会触发仓位核对暂停。
    assert_eq!(second.position, Position::parse("0.02").unwrap());
    assert_eq!(second.expected_position, Position::parse("0.02").unwrap());

    // 后续成交把累计量推到 0.03：只补 0.01 的差额。
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.01").unwrap(), at(3))
        .unwrap();
    let third = harness.step(3).await;

    assert_eq!(third.applied_fills, 1);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.02").unwrap()
    );
    assert_eq!(third.position, Position::parse("0.03").unwrap());
    assert_eq!(third.expected_position, Position::parse("0.03").unwrap());
    // 重复投递的那笔只留一条：流水表里是两笔真实成交加上这一步的新成交。
    let final_fills = harness.runtime.fills(&harness.strategy_id).unwrap();
    assert_eq!(
        final_fills.len(),
        3,
        "the duplicate report must not add a row"
    );
    // 按成交编号去重后的数量之和正好是累计成交量：这张单的成交对完账了。
    let order = harness
        .orders()
        .into_iter()
        .find(|order| order.record.client_order_id == buy)
        .unwrap();
    assert!(order.fills_reconciled);
}

#[tokio::test]
async fn a_late_fill_older_than_the_sync_point_is_still_recorded() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 同步点已经推到 at(2)。
    harness.step(2).await;

    // 这时才收到一笔时间戳是 at(1) 的成交：只按同步点查询就会永久漏掉它。
    harness
        .exchange
        .inject_fill(
            "trade-late",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(1),
        )
        .unwrap();
    let report = harness.step(3).await;

    assert_eq!(report.applied_fills, 1);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
    // 仓位对得上，不会因为漏记成交而触发仓位核对暂停。
    assert!(!report.is_paused());
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
}

#[tokio::test]
async fn fills_of_other_orders_are_not_recorded() {
    let harness = harness();
    harness.step(0).await;

    // 一张不属于本策略的订单的成交（例如用户手动挂的单）。
    harness.exchange.inject_foreign_fill(
        "trade-foreign",
        &poise_strategy::order::ClientOrderId::try_new("0xforeign").unwrap(),
        Price::parse("95000").unwrap(),
        Quantity::parse("0.05").unwrap(),
        at(1),
    );
    let report = harness.step(1).await;

    assert_eq!(report.applied_fills, 0);
    assert!(
        harness
            .runtime
            .fills(&harness.strategy_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.05").unwrap()
    );
}
