//! 系统状态闸门与订单结果未知（第 7.2、9.2 节，B14、B15、B16、B32、B33、B36）。
//!
//! 系统状态不对，本策略就不发任何新订单；撤单始终允许，已在场的挂单保留。
//!
//! 结果未知的订单先按 `cloid` 查询：查到了就按状态处理；查不到时，期限（`expiresAfter`
//! 加安全余量）过后挂单、成交、仓位三项核对一致才作废、释放预留。
//!
//! 行情和成交都是每一步向交易所现查的（没有推送通道）：查不到时这一步到此为止，不发
//! 新单、不撤单，下一步重新查、重新核对之后才继续（B32、B33）。

mod common;

use chrono::Duration;
use common::{Harness, at, harness, long_cell};
use poise_runtime::{Exchange, OrderRequest, StoredOrder, StrategyStatus, TimeInForce};
use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Side};
use poise_strategy::planning::WaitReason;
use poise_strategy::units::{Price, Quantity};

/// 默认选项下 [`an_exit_that_never_landed`] 那张卖单最早什么时候能作废：第 2 秒发出，
/// `expiresAfter` 5 秒，安全余量 30 秒。
const DEADLINE: i64 = 37;

/// 先正常挂出二十格买单，再让第 0 格买单成交、它的卖单下单超时：库里留下一张结果未知的
/// 卖单（交易所从没收到它），其余各格的买单照常在场。
async fn an_exit_that_never_landed(harness: &Harness) -> StoredOrder {
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let quantity = harness.order(&entry).record.quantity;
    harness.exchange.fill(&entry, quantity, at(1)).unwrap();
    harness.exchange.unknown_next_place("timed out");
    harness.step(2).await;
    let unknown = the_unknown_order(harness);
    assert_eq!(unknown.record.unit, long_cell(0));
    assert_eq!(unknown.record.purpose, OrderPurpose::GridExit);
    unknown
}

/// 库里那张结果未知的订单。
fn the_unknown_order(harness: &Harness) -> StoredOrder {
    harness
        .orders()
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Unknown)
        .expect("one order must have an unknown result")
}

/// 另找一格还在场的网格买单：用它的成交制造「这一步本来要发新单」。
fn another_entry(harness: &Harness, besides: &StoredOrder) -> StoredOrder {
    harness
        .working()
        .into_iter()
        .find(|order| {
            order.record.purpose == OrderPurpose::GridEntry
                && order.record.unit != besides.record.unit
                && order.record.status == OrderStatus::Open
        })
        .expect("another grid entry must be working")
}

/// B14：回执丢了，可请求已经落地。按 `cloid` 查到它在场，结束未知状态，照常运行。
#[tokio::test]
async fn b14_a_lost_receipt_is_resolved_by_the_cloid_lookup() {
    let harness = harness().await;
    harness.exchange.lose_next_place_response("timed out");
    harness.step(0).await;
    let lost = the_unknown_order(&harness);

    let report = harness.step(1).await;

    assert_eq!(
        harness.order(&lost.record.client_order_id).record.status,
        OrderStatus::Open
    );
    assert!(
        !report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
}

/// B14：回执丢了，查到它已经成交：按成交记账，结束未知状态。
#[tokio::test]
async fn b14_a_lost_receipt_of_a_filled_order_is_booked() {
    let harness = harness().await;
    harness.exchange.lose_next_place_response("timed out");
    harness.step(0).await;
    let lost = the_unknown_order(&harness);
    let to_buy_back_before = harness.cell(cell_index(&lost)).to_buy_back;

    harness
        .exchange
        .fill(&lost.record.client_order_id, lost.record.quantity, at(1))
        .unwrap();
    let report = harness.step(2).await;

    let booked = harness.order(&lost.record.client_order_id);
    assert_eq!(booked.record.status, OrderStatus::Filled);
    assert_eq!(booked.record.cumulative_filled, lost.record.quantity);
    assert_eq!(
        harness.cell(cell_index(&lost)).to_buy_back,
        to_buy_back_before.saturating_sub(lost.record.quantity)
    );
    assert!(
        !report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
}

/// B36、B15：查不到的订单有结论之前，本策略不发任何新订单，已在场的挂单保留；期限
/// 过后三项核对一致，它作废、释放预留，闸门打开。作废的单不换 `cloid` 重发：它的单位
/// 按状态重新规划，下一张是新订单。
#[tokio::test]
async fn b15_b36_an_order_that_never_landed_closes_the_gate_until_it_is_voided() {
    let harness = harness().await;
    let unknown = an_exit_that_never_landed(&harness).await;
    let entry = another_entry(&harness, &unknown);
    let working = harness.working().len();

    // 另一格的买单成交了：照常要挂出它的卖单，可闸门关着。
    harness
        .exchange
        .fill(&entry.record.client_order_id, entry.record.quantity, at(3))
        .unwrap();
    let report = harness.step(4).await;

    assert!(report.placed.is_empty(), "闸门关着，一张新单都不发");
    assert!(report.canceled.is_empty(), "已在场的挂单保留");
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    assert!(
        report
            .plan
            .place
            .iter()
            .any(|intent| intent.unit == entry.record.unit
                && intent.purpose == OrderPurpose::GridExit),
        "规划照常算出卖单，页面上看得到闸门打开之后会发什么"
    );
    assert_eq!(harness.working().len(), working - 1);
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap_or_default();
    assert!(note.contains("结果未知"), "unexpected note: {note}");
    assert!(note.contains("等期限"), "unexpected note: {note}");

    // 期限还没过：请求可能还在路上，不能作废。
    let report = harness.step(DEADLINE - 1).await;
    assert!(report.placed.is_empty());
    assert_eq!(
        harness.order(&unknown.record.client_order_id).record.status,
        OrderStatus::Unknown
    );

    // 期限过了，挂单里没有它、没有它的成交、仓位一致：作废，闸门打开。
    let report = harness.step(DEADLINE).await;

    let voided = harness.order(&unknown.record.client_order_id);
    assert_eq!(voided.record.status, OrderStatus::Void);
    assert!(voided.fills_reconciled);
    assert!(
        !report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    let placed: Vec<StoredOrder> = report
        .placed
        .iter()
        .map(|client_order_id| harness.order(client_order_id))
        .collect();
    assert!(
        placed
            .iter()
            .any(|order| order.record.unit == entry.record.unit
                && order.record.purpose == OrderPurpose::GridExit),
        "成交那一格的卖单发出去了"
    );
    let replacement = placed
        .iter()
        .find(|order| {
            order.record.unit == unknown.record.unit
                && order.record.purpose == unknown.record.purpose
        })
        .expect("the voided order's unit is planned again");
    assert_ne!(
        replacement.record.client_order_id, unknown.record.client_order_id,
        "作废的单不重发，下一张是新订单"
    );
    assert!(
        harness
            .runtime
            .detail(&harness.strategy_id)
            .unwrap()
            .note
            .is_none()
    );
}

/// 回执丢了，查到它已经成交、可流水还没到：它落过地，不能按「从没生效」作废。
///
/// 按 `cloid` 查到 `Filled` 就按订单状态记账、记下交易所的订单编号，结束未知状态，不等
/// 流水（第 9.3 节）；它的卖单这一步就挂出去。流水后来到了只进流水表，不重复记账。
#[tokio::test]
async fn a_filled_order_with_late_fills_is_not_voided() {
    let harness = harness().await;
    harness.exchange.lose_next_place_response("timed out");
    harness.step(0).await;
    let lost = the_unknown_order(&harness);
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&lost.record.client_order_id, lost.record.quantity, at(1))
        .unwrap();

    let report = harness.step(DEADLINE + 5).await;

    let order = harness.order(&lost.record.client_order_id);
    assert_eq!(order.record.status, OrderStatus::Filled);
    assert_eq!(order.record.cumulative_filled, lost.record.quantity);
    assert!(order.exchange_order_id.is_some());
    assert!(
        report.placed.iter().any(|client_order_id| {
            let placed = harness.order(client_order_id);
            placed.record.unit == lost.record.unit
                && placed.record.purpose == OrderPurpose::GridExit
        }),
        "booked from its status, its sell goes out in the same step"
    );

    harness.exchange.deliver_delayed_fills();
    let report = harness.step(DEADLINE + 6).await;

    assert_eq!(report.applied_fills, 0, "the late fill is not booked twice");
    assert_eq!(
        harness
            .order(&lost.record.client_order_id)
            .record
            .cumulative_filled,
        lost.record.quantity
    );
}

/// 确认在场过的挂单后来查不到了：可能只是查询滞后，它仍然可能成交。`expiresAfter` 只限制
/// 请求送达的时间，不撤销已经挂上的单，所以不按期限作废，等交易所给出明确的结局。
#[tokio::test]
async fn an_order_the_exchange_once_confirmed_is_not_voided_when_it_disappears() {
    let harness = harness().await;
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness.exchange.hide_from_queries(&entry);

    let report = harness.step(600).await;

    assert_eq!(harness.order(&entry).record.status, OrderStatus::Unknown);
    assert!(report.placed.is_empty());
    assert!(report.canceled.is_empty());
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap_or_default();
    assert!(note.contains("曾确认在场"), "unexpected note: {note}");

    // 查询追上来了：它还在场，结束未知状态。
    harness.exchange.show_in_queries(&entry);
    let report = harness.step(601).await;

    assert_eq!(harness.order(&entry).record.status, OrderStatus::Open);
    assert!(
        !report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
}

/// B32：行情是一步开头读的，发单途中过了有效期就不再发新单，剩下的下一步按新行情再发。
#[tokio::test]
async fn b32_new_orders_stop_once_the_market_price_is_stale() {
    let harness = harness().await;
    // 每发一张单花 4 秒：发完第三张时行情已经读了 12 秒，超过 10 秒的有效期。
    harness
        .exchange
        .slow_places(harness.clock.clone(), Duration::seconds(4));

    let report = harness.step(0).await;

    assert_eq!(report.placed.len(), 3);
    assert!(report.plan.waiting.contains(&WaitReason::StaleMarketData));

    harness
        .exchange
        .slow_places(harness.clock.clone(), Duration::zero());
    let report = harness.step(20).await;
    assert_eq!(report.placed.len(), 17);
}

/// B32：行情响应在路上耽搁太久，收到时已经过期：年龄从发出请求算起，这一步一张新单都不发。
#[tokio::test]
async fn b32_a_slow_market_price_response_counts_as_stale() {
    let harness = harness().await;
    harness
        .exchange
        .slow_mark_price(harness.clock.clone(), Duration::seconds(11));

    let report = harness.step(0).await;

    assert!(report.placed.is_empty());
    assert!(report.plan.waiting.contains(&WaitReason::StaleMarketData));
}

/// B16：三项里缺一项就不释放，本策略继续不发新单，页面上写清楚缺的是哪一项（第 9.2 节
/// 第 4 条）。这里缺的是「没有成交」：交易所查不到它，可它成交了一部分——它落过地，
/// 剩下的部分可能还挂在交易所上。（全部成交时成交流水本身就是结局，记账后它就是已成交。）
#[tokio::test]
async fn b16_an_unknown_order_with_fills_is_never_voided() {
    let harness = harness().await;
    harness.exchange.lose_next_place_response("timed out");
    harness.step(0).await;
    let lost = the_unknown_order(&harness);
    harness
        .exchange
        .hide_from_queries(&lost.record.client_order_id);
    let half = Quantity::from_units(lost.record.quantity.units() / 2);
    harness
        .exchange
        .fill(&lost.record.client_order_id, half, at(1))
        .unwrap();

    let report = harness.step(DEADLINE + 5).await;

    assert_eq!(
        harness.order(&lost.record.client_order_id).record.status,
        OrderStatus::Unknown
    );
    assert!(report.placed.is_empty());
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap_or_default();
    assert!(note.contains("有成交记录"), "unexpected note: {note}");
    assert!(!note.contains("等期限"), "unexpected note: {note}");
}

/// 闸门只管新单：规划出来的撤单照常执行（第 7.2 节「撤单始终允许」）。
///
/// 一张在场的旧买单挂错了价，规划要撤掉它、换一张新的；同时另一张单结果未知。撤单照撤，
/// 替换的那张等闸门打开再发。
#[tokio::test]
async fn planned_cancels_still_go_out_while_the_gate_is_closed() {
    let harness = harness().await;
    let unknown = an_exit_that_never_landed(&harness).await;
    let entry = another_entry(&harness, &unknown);

    // 那一格的买单撤掉，换成一张挂错价的旧单：库里和交易所上都有它。
    harness
        .exchange
        .cancel_order(harness.exchange.instrument(), &entry.record.client_order_id)
        .await
        .unwrap();
    let mut stale = harness.order(&entry.record.client_order_id);
    stale.record.status = OrderStatus::Canceled;
    stale.fills_reconciled = true;
    harness
        .runtime
        .store()
        .update_order(&harness.strategy_id, &stale)
        .unwrap();
    let wrong_price = ClientOrderId::try_new("0xwrongprice").unwrap();
    let record = OrderRecord {
        client_order_id: wrong_price.clone(),
        limit_price: Price::parse("80000").unwrap(),
        status: OrderStatus::Open,
        ..entry.record.clone()
    };
    harness
        .runtime
        .store()
        .insert_order(
            &harness.strategy_id,
            &StoredOrder {
                record: record.clone(),
                fills_reconciled: true,
                ..entry.clone()
            },
        )
        .unwrap();
    harness
        .exchange
        .place_order(&OrderRequest {
            instrument: harness.exchange.instrument().clone(),
            client_order_id: wrong_price.clone(),
            side: Side::Buy,
            limit_price: record.limit_price,
            quantity: record.quantity,
            reduce_only: false,
            time_in_force: TimeInForce::GoodTillCanceled,
            expires_at: at(10),
        })
        .await
        .unwrap();

    let report = harness.step(3).await;

    assert_eq!(report.canceled, vec![wrong_price.clone()]);
    assert_eq!(
        harness.order(&wrong_price).record.status,
        OrderStatus::Canceled
    );
    assert!(report.placed.is_empty(), "替换的那张等闸门打开再发");
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Running
    );
}

/// 暂停、恢复同样靠三项核对收场：撤销只会得到「查无此单」，期限过后作废，恢复才进入
/// 运行中，重新挂单。
#[tokio::test]
async fn a_resume_goes_on_once_the_unknown_leftover_is_voided() {
    let harness = harness().await;
    let unknown = an_exit_that_never_landed(&harness).await;
    let place_count = harness.exchange.place_count();

    harness.pause(8).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Pausing,
        "期限没过，这张单还没收场"
    );
    harness.resume(9).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Restoring
    );
    assert_eq!(harness.exchange.place_count(), place_count);

    let report = harness.step(DEADLINE).await;

    assert_eq!(report.status, StrategyStatus::Running);
    assert_eq!(
        harness.order(&unknown.record.client_order_id).record.status,
        OrderStatus::Void
    );
    assert_eq!(
        report.placed.len(),
        20,
        "十九格的买单和第 0 格的卖单重新挂出"
    );
}

/// 停止也一样：结果未知的那张作废之后才归档。
#[tokio::test]
async fn a_stop_archives_once_the_unknown_order_is_voided() {
    let harness = harness().await;
    an_exit_that_never_landed(&harness).await;

    harness.stop(8).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopping
    );

    let report = harness.step(DEADLINE).await;

    assert_eq!(report.status, StrategyStatus::Stopped);
}

/// B32：读不到行情时这一步不发新单，已在场的挂单保留；行情恢复后继续。
#[tokio::test]
async fn b32_no_new_orders_without_a_fresh_market_price() {
    let harness = harness().await;
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let quantity = harness.order(&entry).record.quantity;
    harness.exchange.fill(&entry, quantity, at(1)).unwrap();
    let working = working_at_exchange(&harness);
    let place_count = harness.exchange.place_count();

    harness.exchange.fail_next_mark_price("行情接口不可用");
    harness.clock.set(at(2));
    assert!(harness.runtime.step(&harness.strategy_id).await.is_err());

    assert_eq!(harness.exchange.place_count(), place_count, "不发新单");
    // 读行情之前这一步已经补记了那笔成交（订单在库里转成已成交），交易所上的挂单
    // 一张都没撤。
    assert_eq!(working_at_exchange(&harness), working, "已在场的挂单保留");

    // 行情恢复：这一格的卖单照常挂出。
    let report = harness.step(3).await;
    assert!(report.placed.iter().any(|client_order_id| {
        let order = harness.order(client_order_id);
        order.record.unit == long_cell(0) && order.record.purpose == OrderPurpose::GridExit
    }));
}

/// B33：成交查不到时，重新核对订单、成交和仓位之前不发新订单；核对一致后继续。
#[tokio::test]
async fn b33_no_new_orders_until_the_fills_are_read_again() {
    let harness = harness().await;
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let quantity = harness.order(&entry).record.quantity;
    harness.exchange.fill(&entry, quantity, at(1)).unwrap();
    let working = harness.working().len();
    let place_count = harness.exchange.place_count();

    harness.exchange.fail_fills_since(1, "成交接口不可用");
    harness.clock.set(at(2));
    assert!(harness.runtime.step(&harness.strategy_id).await.is_err());

    assert_eq!(harness.exchange.place_count(), place_count, "不发新单");
    assert_eq!(harness.working().len(), working, "已在场的挂单保留");
    // 页面只读视图快照：这一步没走通也要写清楚，不能一直显示上一步的「没有等待」。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.waiting, vec![WaitReason::FillsPending]);
    let note = detail.note.unwrap_or_default();
    assert!(
        note.contains("没读到") && note.contains("成交接口不可用"),
        "{note}"
    );

    let report = harness.step(3).await;
    assert_eq!(report.applied_fills, 1, "重新核对时补记那笔成交");
    assert!(report.placed.iter().any(|client_order_id| {
        let order = harness.order(client_order_id);
        order.record.unit == long_cell(0) && order.record.purpose == OrderPurpose::GridExit
    }));
}

/// 交易所上还在场的挂单张数。
fn working_at_exchange(harness: &Harness) -> usize {
    harness
        .exchange
        .orders()
        .iter()
        .filter(|order| order.status.is_working())
        .count()
}

fn cell_index(order: &StoredOrder) -> usize {
    (0..20)
        .find(|index| long_cell(*index) == order.record.unit)
        .expect("the order belongs to a long cell")
}
