//! 资金预检与保证金不足的退避（第 10 节，B20、B29）。
//!
//! 每张会增加敞口的单下单前做一次保守预检：所需保证金（数量 × 限价 ÷ 杠杆 + taker 手续费）
//! 不超过可用资金减去本系统在场同类订单的估算。交易所以保证金不足拒绝后，这个单位按
//! 1、2、4、8 秒……退避；只减仓单不受影响。

mod common;

use common::{Harness, at, harness};
use poise_strategy::order::OrderPurpose;
use poise_strategy::planning::WaitReason;
use poise_strategy::units::Notional;

/// 默认样例：二十格买单，每格 .05，买价 95000 到 76000；模拟账户杠杆 10 倍。
/// 每格占用约 .05 × 价格 ÷ 10，二十格合计约 8,550 加手续费。
async fn started() -> Harness {
    let harness = harness().await;
    harness.step(0).await;
    harness
}

/// 第 0 格买单整份成交：下一步要挂出它的卖单（只减仓），并补回这一格的买单要等卖出。
async fn fill_cell(harness: &Harness, cell: usize, seconds: i64) {
    let entry = harness.working_cloid(cell, OrderPurpose::GridEntry);
    let quantity = harness.order(&entry).record.quantity;
    harness
        .exchange
        .fill(&entry, quantity, at(seconds))
        .unwrap();
}

/// 可用资金不够：预检挡住放不下的增仓单，不发出去，页面上写「资金不足」。
#[tokio::test]
async fn an_order_that_does_not_fit_is_not_sent() {
    let harness = harness().await;
    // 只够第一格：95000 × .05 ÷ 10 = 475，加手续费约 2.1。
    harness
        .account
        .set_collateral(Notional::parse("500").unwrap());

    let report = harness.step(0).await;

    assert_eq!(report.placed.len(), 1);
    assert_eq!(harness.exchange.place_count(), 1);
    assert!(report.plan.waiting.contains(&WaitReason::InsufficientFunds));
}

/// 已经在场的增仓单占着额度：下一步不会再按全额放新单。
#[tokio::test]
async fn working_orders_hold_their_share_of_the_funds() {
    let harness = harness().await;
    harness
        .account
        .set_collateral(Notional::parse("1000").unwrap());
    let report = harness.step(0).await;
    // 1000 只够前两格（475 + 470 加手续费）。
    assert_eq!(report.placed.len(), 2);

    let report = harness.step(1).await;

    assert!(report.placed.is_empty(), "在场的两格已经占满额度");
}

/// B29：用户在现货上的买卖改变了余额，下一次预检按新余额算；这不会让仓位核对暂停策略。
#[tokio::test]
async fn b29_the_next_pre_check_uses_the_new_balance() {
    let harness = harness().await;
    harness
        .account
        .set_collateral(Notional::parse("500").unwrap());
    let report = harness.step(0).await;
    assert_eq!(report.placed.len(), 1);

    // 用户在别处卖掉现货，可用资金多了：下一步把剩下的买单补上。
    harness
        .account
        .set_collateral(Notional::parse("100000").unwrap());
    let report = harness.step(1).await;

    assert_eq!(report.placed.len(), 19);
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        poise_runtime::StrategyStatus::Running
    );
}

/// B20：交易所以保证金不足拒绝，按明确拒绝处理并退避 1、2、4 秒……；卖单不受影响。
#[tokio::test]
async fn b20_a_margin_rejection_backs_off_and_leaves_the_sells_alone() {
    let harness = started().await;
    // 第 0 格买入、卖出：这一格要补回买单。
    fill_cell(&harness, 0, 1).await;
    harness.step(2).await;
    let exit = harness.working_cloid(0, OrderPurpose::GridExit);
    let quantity = harness.order(&exit).record.quantity;
    harness.exchange.fill(&exit, quantity, at(3)).unwrap();
    // 第 1 格买入：这一步要挂出它的卖单（只减仓）。
    fill_cell(&harness, 1, 3).await;

    // 补回的买单被以保证金不足拒绝；卖单照常发出。
    harness.exchange.reject_for_margin(1);
    let report = harness.step(4).await;
    let placed: Vec<_> = report
        .placed
        .iter()
        .map(|cloid| harness.order(cloid))
        .collect();
    let rejected = placed
        .iter()
        .find(|order| order.record.purpose == OrderPurpose::GridEntry)
        .expect("the buy back was sent");
    assert_eq!(
        rejected.record.status,
        poise_strategy::order::OrderStatus::Rejected
    );
    assert!(
        placed
            .iter()
            .any(|order| order.record.purpose == OrderPurpose::GridExit
                && order.record.status != poise_strategy::order::OrderStatus::Rejected),
        "卖单不受保证金不足影响"
    );
    assert!(report.plan.waiting.contains(&WaitReason::InsufficientFunds));
    let cell_zero_entry = |report: &poise_runtime::StepReport| {
        report.placed.iter().any(|cloid| {
            let order = harness.order(cloid);
            order.record.unit == common::long_cell(0)
                && order.record.purpose == OrderPurpose::GridEntry
        })
    };

    // 第一次退避 1 秒：同一秒里再走一步，第 0 格不再撞一次。（第 1 格的卖单挂上就成交了，
    // 它补回的买单不在退避里，照常发出。）
    let report = harness.step(4).await;
    assert!(!cell_zero_entry(&report), "退避期内不重发");
    assert!(report.plan.waiting.contains(&WaitReason::InsufficientFunds));

    // 1 秒之后重试，这次成功，退避结束。
    let report = harness.step(5).await;
    assert!(cell_zero_entry(&report));
    assert!(!report.plan.waiting.contains(&WaitReason::InsufficientFunds));
}

/// 连续被拒时退避加倍：第二次被拒之后要等 2 秒。
#[tokio::test]
async fn repeated_margin_rejections_double_the_wait() {
    let harness = started().await;
    fill_cell(&harness, 0, 1).await;
    harness.step(2).await;
    let exit = harness.working_cloid(0, OrderPurpose::GridExit);
    let quantity = harness.order(&exit).record.quantity;
    harness.exchange.fill(&exit, quantity, at(3)).unwrap();

    harness.exchange.reject_for_margin(2);
    harness.step(4).await; // 第一次被拒：等 1 秒
    harness.step(5).await; // 第二次被拒：等 2 秒
    let place_count = harness.exchange.place_count();

    harness.step(6).await;
    assert_eq!(
        harness.exchange.place_count(),
        place_count,
        "还在 2 秒的退避里"
    );

    let report = harness.step(7).await;
    assert!(
        report
            .placed
            .iter()
            .any(|cloid| harness.order(cloid).record.purpose == OrderPurpose::GridEntry)
    );
}

/// 普通的明确拒单（不是保证金不足）同样不占额度：后面放得下的单照常发。
#[tokio::test]
async fn an_ordinary_rejection_gives_its_funds_back() {
    let harness = harness().await;
    // 只够一格：第一格被拒之后，这份额度给第二格用。
    harness
        .account
        .set_collateral(Notional::parse("500").unwrap());
    harness
        .exchange
        .reject_next_place("price too far from the oracle");

    let report = harness.step(0).await;

    let orders: Vec<_> = report
        .placed
        .iter()
        .map(|cloid| harness.order(cloid))
        .collect();
    assert_eq!(orders.len(), 2);
    assert_eq!(
        orders[0].record.status,
        poise_strategy::order::OrderStatus::Rejected
    );
    assert_eq!(
        orders[1].record.status,
        poise_strategy::order::OrderStatus::Open
    );
}

/// 退避从收到拒绝的那一刻起算，不从这一步开始的时刻算：一步里花掉的时间不能吃掉退避。
#[tokio::test]
async fn the_backoff_starts_when_the_rejection_arrives() {
    let harness = harness().await;
    // 每发一张单花 2 秒：第一张在第 2 秒被拒，退避到第 3 秒。
    harness
        .exchange
        .slow_places(harness.clock.clone(), chrono::Duration::seconds(2));
    harness.exchange.reject_for_margin(1);
    let report = harness.step(0).await;
    let rejected = harness.order(&report.placed[0]);
    assert_eq!(
        rejected.record.status,
        poise_strategy::order::OrderStatus::Rejected
    );
    harness
        .exchange
        .slow_places(harness.clock.clone(), chrono::Duration::zero());

    let report = harness.step(2).await;
    assert!(
        report
            .placed
            .iter()
            .all(|cloid| harness.order(cloid).record.unit != rejected.record.unit),
        "第 2 秒还在退避里"
    );

    let report = harness.step(3).await;
    assert!(
        report
            .placed
            .iter()
            .any(|cloid| harness.order(cloid).record.unit == rejected.record.unit)
    );
}

/// 同一步里既有增仓单又有只减仓卖单，资金查询失败：增仓单这一步不发，卖单照常发出。
#[tokio::test]
async fn reduce_only_orders_go_out_even_when_the_funds_check_fails() {
    let harness = started().await;
    // 第 0 格买入：这一步要挂出它的卖单（只减仓）。第 5 格的买单被撤掉：这一步要补挂（增仓）。
    fill_cell(&harness, 0, 1).await;
    let entry = harness.working_cloid(5, OrderPurpose::GridEntry);
    assert!(harness.exchange.cancel_externally(&entry));
    harness.account.fail_leverage(1);

    let report = harness.step(2).await;

    let placed: Vec<_> = report
        .placed
        .iter()
        .map(|cloid| harness.order(cloid))
        .collect();
    assert!(
        placed
            .iter()
            .any(|order| order.record.purpose == OrderPurpose::GridExit),
        "卖单照常发出"
    );
    assert!(
        placed
            .iter()
            .all(|order| order.record.purpose != OrderPurpose::GridEntry),
        "增仓单这一步不发"
    );
    assert!(report.plan.waiting.contains(&WaitReason::InsufficientFunds));
}
