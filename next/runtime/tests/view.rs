//! 只读视图：策略列表与运行详情（第 11 节）。
//!
//! 网页只显示服务端给的数据，自己不算交易数量。所以这里要确认详情里的批次表、
//! 格子表、在场挂单和等待原因都是服务端算好的。
//!
//! 列表和详情都只读库：行情、仓位、可用资金和等待原因来自后台每一步之后落的视图
//! 快照。读一次不会调交易所，也不会抢驱动锁。

mod common;

use std::time::Duration;

use common::{at, harness, harness_with, long_grid_config, throttled};
use poise_runtime::StrategyStatus;
use poise_strategy::order::OrderPurpose;
use poise_strategy::planning::WaitReason;
use poise_strategy::preview::CellStatus;
use poise_strategy::units::{Notional, Position, Price, Quantity};

/// 列表：标的、状态、仓位、在场挂单数和当前参数版本。
#[tokio::test]
async fn the_list_shows_the_instrument_status_position_and_working_orders() {
    let console = common::console();
    assert!(console.runtime.strategies().unwrap().is_empty());

    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console.clock.set(at(1));
    console.runtime.step(&draft_id).await.unwrap();

    let strategies = console.runtime.strategies().unwrap();
    assert_eq!(strategies.len(), 1);
    let summary = &strategies[0];
    assert_eq!(summary.strategy_id, draft_id);
    assert_eq!(summary.instrument, common::btc());
    assert_eq!(summary.status, StrategyStatus::Running);
    assert_eq!(summary.config_version, 1);
    assert_eq!(summary.working_orders, 20);
    assert_eq!(summary.position, Position::ZERO);
    assert_eq!(summary.config, long_grid_config());
}

/// 列表按新建的在前。一个标的只能有一个活动策略，所以第二个要等第一个停止之后。
#[tokio::test]
async fn the_list_is_newest_first() {
    let console = common::console();
    let first = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&first).await.unwrap();
    console
        .runtime
        .start_draft(&first, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console.clock.set(at(10));
    console.runtime.stop(&first).await.unwrap();

    let second = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&second).await.unwrap();
    console
        .runtime
        .start_draft(&second, preview.token.as_deref().unwrap())
        .await
        .unwrap();

    let strategies = console.runtime.strategies().unwrap();
    let ids: Vec<_> = strategies
        .iter()
        .map(|summary| summary.strategy_id.clone())
        .collect();
    assert_eq!(ids, vec![second, first]);
}

/// 详情：批次表、格子表、在场挂单和参数版本。
#[tokio::test]
async fn the_detail_shows_the_tables_the_orders_and_the_config_versions() {
    let console = common::console();
    let draft_id = console
        .runtime
        .create_draft(common::core_long_config())
        .unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console.clock.set(at(1));
    console.runtime.step(&draft_id).await.unwrap();

    let detail = console.runtime.detail(&draft_id).unwrap();
    assert_eq!(detail.summary.strategy_id, draft_id);
    assert_eq!(detail.market_price, Price::parse("95500").unwrap());
    assert_eq!(detail.batches.len(), 4);
    assert_eq!(detail.cells.len(), 20);
    // 价格还在最高买价之上：挂出去的是 4 批主仓买单，格子等着各自的批次。
    assert_eq!(detail.orders.len(), 4);
    assert!(detail.orders.iter().all(|order| order.record.is_working()));
    assert_eq!(
        detail
            .cells
            .iter()
            .filter(|cell| cell.status == CellStatus::WaitingForBatch)
            .count(),
        20
    );
    assert_eq!(detail.config_versions.len(), 1);
    assert_eq!(detail.config_versions[0].version, 1);
    assert_eq!(detail.position, Position::ZERO);
    assert_eq!(detail.expected_position, Position::ZERO);
    assert_eq!(
        detail.available_collateral,
        Notional::parse("100000").unwrap()
    );
}

/// 等待原因：没放行的批次在等限速，格子等着各自的批次。
#[tokio::test]
async fn the_detail_says_what_each_unit_is_waiting_for() {
    let harness = harness_with(
        throttled(common::core_long_config(), chrono::Duration::hours(1)),
        poise_runtime::Store::open_in_memory().unwrap(),
    )
    .await;
    harness.step(0).await;

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert!(
        detail.waiting.contains(&WaitReason::WaitingForThrottle),
        "unexpected waiting reasons: {:?}",
        detail.waiting
    );
    // 只有第 0 批放行了，它的 5 格等着这一批主仓买入。
    assert_eq!(detail.orders.len(), 1);
    assert_eq!(
        detail
            .cells
            .iter()
            .filter(|cell| cell.status == CellStatus::WaitingForBatch)
            .count(),
        20
    );
}

/// 详情是只读的：读一次不碰交易所，也不写库。
///
/// 页面每隔几秒刷新一次，读详情要是顺手去问交易所，读得越勤交易所调用越多；写入还会
/// 和常驻服务抢同一把驱动锁。
#[tokio::test]
async fn reading_the_detail_changes_nothing() {
    let harness = harness().await;
    harness.step(0).await;
    let placed = harness.exchange.place_count();
    let working: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();

    harness.exchange.reset_call_count();
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let summary = harness.runtime.strategies().unwrap();

    assert_eq!(
        harness.exchange.call_count(),
        0,
        "读详情和列表不该调用交易所"
    );
    assert_eq!(detail.orders.len(), working.len());
    assert_eq!(harness.exchange.place_count(), placed);
    let after: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();
    assert_eq!(after, working);
    assert_eq!(summary.len(), 1);
}

/// 详情里的摘要和订单表说的是同一次读取的结果。
#[tokio::test]
async fn the_summary_agrees_with_the_rest_of_the_detail() {
    let harness = harness().await;
    harness.step(0).await;
    // 一张单在交易所上全部成交了，下一步才会记进库。
    let filled = harness.working_cloid(0, OrderPurpose::GridBuy);
    let quantity = harness.order(&filled).record.quantity;
    harness.exchange.fill(&filled, quantity, at(10)).unwrap();
    harness.step(11).await;

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();

    assert!(
        !detail
            .orders
            .iter()
            .any(|order| order.record.client_order_id == filled),
        "成交的那张单不再在场"
    );
    assert_eq!(detail.orders.len(), 20, "这一格买满之后挂出了卖单");
    assert_eq!(
        detail.summary.working_orders,
        detail.orders.len(),
        "摘要里的在场订单数和订单表说的是同一件事"
    );
    assert_eq!(detail.position, Position::parse("0.05").unwrap());
    assert_eq!(detail.summary.position, detail.position);
}

/// 详情里能看到持有与待买回：一格买回一半之后。
#[tokio::test]
async fn the_detail_follows_a_cell_through_its_cycle() {
    let harness = harness().await;
    harness.step(0).await;
    let cell0_buy = harness.working_cloid(0, OrderPurpose::GridBuy);
    harness
        .exchange
        .fill(&cell0_buy, Quantity::parse("0.02").unwrap(), at(10))
        .unwrap();
    harness.step(11).await;

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let cell0 = &detail.cells[0];
    assert_eq!(cell0.held, Quantity::parse("0.02").unwrap());
    assert_eq!(cell0.to_buy_back, Quantity::parse("0.03").unwrap());
    assert_eq!(cell0.buy_price, Price::parse("95000").unwrap());
    assert_eq!(cell0.sell_price, Price::parse("96000").unwrap());
    // 持有之后：卖单和买回单各一张。
    assert_eq!(cell0.working_orders, 2);
}

/// 正好有一歩在走的时候读详情：照读不误，不等这一步，也不报「忙」。
///
/// 常驻服务每隔几秒走一步，网页每隔几秒刷新一次详情，撞上是常事。读详情只读库、不抢
/// 驱动锁，所以这一步还在走的时候它照样立刻返回上一份快照。
#[tokio::test]
async fn reading_the_detail_does_not_wait_for_a_step_in_flight() {
    let (runtime, clock) = common::slow_runtime(Duration::from_millis(300));
    let draft_id = runtime.create_draft(long_grid_config()).unwrap();
    let preview = runtime.preview_draft(&draft_id).await.unwrap();
    runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    clock.set(at(1));

    // 让这一步停在标记价那次等待上：驱动权还握在它手里。
    let stepping = tokio::spawn({
        let runtime = runtime.clone();
        let strategy_id = draft_id.clone();
        async move { runtime.step(&strategy_id).await }
    });
    tokio::task::yield_now().await;

    // 驱动权确实被占着：这一步走完之前谁也拿不到。
    assert!(
        matches!(
            runtime.step(&draft_id).await,
            Err(poise_runtime::RuntimeError::Busy(_))
        ),
        "the step in flight must hold the driver lock"
    );

    let detail = tokio::time::timeout(Duration::from_secs(5), async {
        runtime.detail(&draft_id).unwrap()
    })
    .await
    .expect("读详情不该被这一步挡住");
    assert_eq!(detail.summary.strategy_id, draft_id);
    assert_eq!(detail.summary.status, StrategyStatus::Running);

    stepping.await.unwrap().unwrap();
}

/// 暂停这类操作也一样：点下去要等这一步走完，而不是报「忙」。
#[tokio::test]
async fn pausing_waits_for_a_step_in_flight() {
    let (runtime, clock) = common::slow_runtime(Duration::from_millis(300));
    let draft_id = runtime.create_draft(long_grid_config()).unwrap();
    let preview = runtime.preview_draft(&draft_id).await.unwrap();
    runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    clock.set(at(1));

    let stepping = tokio::spawn({
        let runtime = runtime.clone();
        let strategy_id = draft_id.clone();
        async move { runtime.step(&strategy_id).await }
    });
    tokio::task::yield_now().await;

    let canceled = tokio::time::timeout(Duration::from_secs(5), runtime.pause(&draft_id))
        .await
        .expect("暂停应该等到这一步走完，而不是一直等下去")
        .expect("这一步还在走，暂停不该报忙");
    // 这一步挂上的单，暂停时全部撤掉。
    assert_eq!(canceled.len(), 20);
    assert_eq!(runtime.status(&draft_id).unwrap(), StrategyStatus::Paused);

    stepping.await.unwrap().unwrap();
}

/// 归档的策略也能看，只是没有什么可等的。
#[tokio::test]
async fn an_archived_strategy_is_still_readable() {
    let harness = harness().await;
    harness.step(0).await;
    harness.stop(1).await;

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.summary.status, StrategyStatus::Stopped);
    assert!(detail.waiting.is_empty());
    assert!(detail.orders.is_empty());
    assert_eq!(detail.cells.len(), 20);
    assert_eq!(detail.summary.working_orders, 0);
}
