//! 进程重启后的恢复（第 12 节）：等 `expiresAfter` 过期、撤销遗留挂单、
//! 补拉成交、核对仓位、按保存的状态重新挂单，不重发旧单。

mod common;

use common::{at, harness_with_store, restart};
use poise_runtime::{ResumeOutcome, Store, StrategyId, StrategyStatus};
use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderStatus};
use poise_strategy::units::{Position, Price, Quantity};

#[tokio::test]
async fn b19_restart_backfills_offline_fills_and_replaces_orders_by_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("poise.sqlite");

    let (strategy_id, exchange, cell0_buy, cell1_buy, old_cloids);
    {
        let harness = harness_with_store(Store::open(&path).unwrap());
        harness.step(0).await;
        assert_eq!(harness.working().len(), 20);
        cell0_buy = harness.working_cloid(0, OrderPurpose::GridBuy);
        cell1_buy = harness.working_cloid(1, OrderPurpose::GridBuy);
        old_cloids = harness
            .orders()
            .into_iter()
            .map(|order| order.record.client_order_id)
            .collect::<Vec<ClientOrderId>>();
        strategy_id = harness.strategy_id.clone();
        exchange = harness.exchange.clone();
        // 进程结束，数据库连接关闭。
    }

    // 进程不在时：95 格整份买回，94 格部分买回；行情跌到 94500。
    exchange
        .fill(&cell0_buy, Quantity::parse("0.05").unwrap(), at(10))
        .unwrap();
    exchange
        .fill(&cell1_buy, Quantity::parse("0.02").unwrap(), at(11))
        .unwrap();
    exchange.set_market_price(Price::parse("94500").unwrap(), at(11));

    let harness = restart(
        Store::open(&path).unwrap(),
        exchange.clone(),
        strategy_id.clone(),
    );

    // 还有可能落地的订单：先等到它们过期，不重复下单。
    let outcome = harness.runtime.resume(&strategy_id, at(1)).await.unwrap();
    assert_eq!(
        outcome,
        ResumeOutcome::WaitingForExpiry { until: at(5) },
        "resume must wait for expiresAfter before touching the exchange"
    );
    assert_eq!(exchange.place_count(), 20);

    let outcome = harness.runtime.resume(&strategy_id, at(12)).await.unwrap();
    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("expected the resume to run a step, got {outcome:?}");
    };

    // 离线期间的成交补记完毕，仓位核对通过。
    assert_eq!(report.applied_fills, 2);
    assert_eq!(report.position, Position::parse("0.07").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.07").unwrap());
    assert_eq!(harness.cell(0).to_buy_back, Quantity::ZERO);
    assert_eq!(
        harness.cell(1).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );

    // 遗留挂单全部撤销，旧订单都到了终态。
    assert!(
        harness
            .working()
            .iter()
            .all(|order| !old_cloids.contains(&order.record.client_order_id))
    );
    let orders = harness.orders();
    let status_of = |client_order_id: &ClientOrderId| {
        orders
            .iter()
            .find(|order| &order.record.client_order_id == client_order_id)
            .unwrap()
            .record
            .status
    };
    assert_eq!(status_of(&cell0_buy), OrderStatus::Filled);
    assert_eq!(status_of(&cell1_buy), OrderStatus::Canceled);
    assert!(
        orders
            .iter()
            .all(|order| order.record.status != OrderStatus::Rejected),
        "no order may be resent with an old client order id"
    );

    // 按状态重新挂单：整份买回的格子挂卖单，部分买回的格子同时挂买回和卖单，
    // 其余格子在自己的买价挂整份买单。
    assert_eq!(report.placed.len(), 21);
    assert!(
        report
            .placed
            .iter()
            .all(|client_order_id| !old_cloids.contains(client_order_id))
    );
    assert_eq!(exchange.place_count(), 41);
    assert_eq!(
        harness
            .at_exchange(&harness.working_cloid(0, OrderPurpose::GridSell))
            .unwrap()
            .limit_price,
        Price::parse("96000").unwrap()
    );
    assert_eq!(
        harness
            .at_exchange(&harness.working_cloid(1, OrderPurpose::GridBuy))
            .unwrap()
            .limit_price,
        Price::parse("94000").unwrap()
    );
    assert_eq!(
        harness
            .at_exchange(&harness.working_cloid(1, OrderPurpose::GridSell))
            .unwrap()
            .limit_price,
        Price::parse("95000").unwrap()
    );
    assert_eq!(
        harness
            .at_exchange(&harness.working_cloid(19, OrderPurpose::GridBuy))
            .unwrap()
            .limit_price,
        Price::parse("76000").unwrap()
    );
    assert_eq!(harness.working().len(), 21);
    assert_eq!(
        harness.runtime.status(&strategy_id).unwrap(),
        StrategyStatus::Running
    );
}

#[tokio::test]
async fn resuming_a_fresh_strategy_does_not_wait_for_expiry() {
    let harness = harness_with_store(Store::open_in_memory().unwrap());

    // 还没有下过单：没有需要等待的订单。
    let outcome = harness
        .runtime
        .resume(&harness.strategy_id, at(0))
        .await
        .unwrap();

    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("expected a step, got {outcome:?}");
    };
    assert_eq!(report.placed.len(), 20);
}

#[tokio::test]
async fn resume_is_refused_for_an_unknown_strategy() {
    let harness = harness_with_store(Store::open_in_memory().unwrap());
    let missing = StrategyId::try_from("s-missing".to_string()).unwrap();

    let error = harness.runtime.resume(&missing, at(0)).await.unwrap_err();

    assert!(matches!(
        error,
        poise_runtime::RuntimeError::Store(poise_runtime::StoreError::UnknownStrategy(_))
    ));
}
