//! 安全底线：先写库再发送、结果未知时保留预留、仓位核对失败就暂停。

mod common;

use common::{at, btc, harness, long_cell, long_grid_config, long_side_mut};
use poise_runtime::{
    ResumeOutcome, Runtime, RuntimeError, SimulatedExchange, Store, StoredOrder, StrategyStatus,
};
use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus};
use poise_strategy::planning::WaitReason;
use poise_strategy::units::{Position, Price, Quantity};

#[tokio::test]
async fn a_rejected_order_is_written_to_the_database_before_it_is_sent() {
    let harness = harness().await;
    harness.exchange.reject_next_place("insufficient margin");

    let report = harness.step(0).await;

    // 20 张单都尝试发送；被拒绝的那张仍然留在数据库里，状态是已拒绝。
    assert_eq!(report.placed.len(), 20);
    let rejected = harness
        .orders()
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Rejected)
        .expect("the rejected order must stay in the database");
    assert!(
        harness
            .at_exchange(&rejected.record.client_order_id)
            .is_none()
    );
    assert_eq!(harness.working().len(), 19);

    // 下一步按状态重新挂上，换新的客户端订单 ID。
    let report = harness.step(1).await;

    assert_eq!(report.placed.len(), 1);
    assert_ne!(report.placed[0], rejected.record.client_order_id);
    assert_eq!(harness.working().len(), 20);
}

#[tokio::test]
async fn an_order_the_exchange_does_not_know_keeps_its_reservation() {
    let harness = harness().await;
    harness.exchange.unknown_next_place("timed out");

    let report = harness.step(0).await;

    // 第一张单结果未知：有结论之前本策略不发任何新订单，这一步剩下的单也不发（第 7.2 节）。
    assert_eq!(report.placed.len(), 1);
    assert_eq!(harness.exchange.place_count(), 1);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    let unknown = harness
        .orders()
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Unknown)
        .expect("the unknown order must stay in the database");

    // 还没过 expiresAfter：保留它的预留，不重发。
    let report = harness.step(1).await;

    assert!(report.placed.is_empty());
    assert_eq!(harness.working().len(), 1);

    // 过了 expiresAfter 也一样：交易所查不到这张单，凭过期就断定它没有影响是不安全的，
    // 所以预留照旧保留、不重发，只在规划里说明原因；还要再等一段安全余量，才按三项核对
    // 作废（第 9.2 节，见 `gate.rs`）。
    let report = harness.step(6).await;

    assert!(report.placed.is_empty());
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    assert_eq!(harness.working().len(), 1);
    assert_eq!(
        harness
            .orders()
            .into_iter()
            .find(|order| order.record.client_order_id == unknown.record.client_order_id)
            .unwrap()
            .record
            .status,
        OrderStatus::Unknown
    );
}

#[tokio::test]
async fn a_cancel_with_an_unknown_result_keeps_the_reservation_and_places_nothing() {
    let harness = harness().await;
    // 数据库里留着一张交易所并不认识的挂单（上一次进程崩溃时的记录）：
    // 撤单只会得到「查无此单」，这不能当成撤销成功。
    let ghost = ClientOrderId::try_new("0xghost").unwrap();
    harness
        .runtime
        .store()
        .insert_order(
            &harness.strategy_id,
            &StoredOrder {
                record: OrderRecord {
                    client_order_id: ghost.clone(),
                    unit: long_cell(0),
                    purpose: OrderPurpose::GridEntry,
                    // 价格与格子买价不符，规划会要求撤掉它、再补一张新的。
                    limit_price: Price::parse("94000").unwrap(),
                    quantity: Quantity::parse("0.05").unwrap(),
                    cumulative_filled: Quantity::ZERO,
                    reduce_only: false,
                    status: OrderStatus::Open,
                },
                fills_reconciled: false,
                exchange_order_id: None,
                expires_at: at(6),
                placed_at: at(0),
                updated_at: at(0),
            },
        )
        .unwrap();
    let place_count = harness.exchange.place_count();

    let report = harness.step(1).await;

    // 规划想撤掉这张单，再补上其余 19 格的买单。第 0 格不补：这张单的成交还没对完账，
    // 它可能已经成交了一部分，再按整份数量补一张就会买多。
    assert_eq!(report.plan.cancel, vec![ghost.clone()]);
    assert_eq!(report.plan.place.len(), 19);
    assert!(
        report
            .plan
            .place
            .iter()
            .all(|intent| intent.unit != long_cell(0))
    );
    assert!(report.plan.waiting.contains(&WaitReason::FillsPending));
    assert!(report.canceled.is_empty());
    assert!(report.placed.is_empty());
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    assert_eq!(harness.exchange.place_count(), place_count);
    // 预留还在：这张单仍然被当作在场，不会重发。
    assert!(
        harness
            .working()
            .iter()
            .any(|order| order.record.client_order_id == ghost)
    );
}

#[tokio::test]
async fn a_position_mismatch_explained_by_an_unrecorded_fill_does_not_pause() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);

    // 交易所已经成交 0.02，但这笔成交的时间戳早于拉取窗口，我们还没记入。
    // 挂单快照里带着这个累计量，仓位差额能由它解释（第 7.1.3 节）。
    harness
        .exchange
        .inject_fill(
            "trade-ancient",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(-600),
        )
        .unwrap();

    let report = harness.step(1).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 0);
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    // 状态没有变：这一步照常挂单，不会因为差额而下单或撤单。
    assert!(report.placed.is_empty());
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.05").unwrap()
    );
}

#[tokio::test]
async fn a_cancel_that_fails_while_pausing_is_retried_by_pausing_again() {
    let harness = harness().await;
    harness.step(0).await;
    harness
        .exchange
        .manual_trade(Quantity::parse("0.01").unwrap());
    // 第一次撤单请求超时：这张单还留在交易所，策略仍然暂停。
    harness.exchange.unknown_next_cancel("timed out");

    let report = harness.step(1).await;

    assert!(report.is_paused());
    assert_eq!(report.canceled.len(), 19);
    assert_eq!(harness.working().len(), 1);

    // 再暂停一次就能把剩下的撤掉：`pause` 可以重复调用。
    let canceled = harness.pause(2).await;

    assert_eq!(canceled.len(), 1);
    assert!(harness.working().is_empty());
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working())
    );
}

#[tokio::test]
async fn pausing_absorbs_a_late_fill_older_than_the_fills_lookback() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 下单当时就成交了 0.02，但成交记录迟迟不到。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.02").unwrap(), at(0))
        .unwrap();
    // 又走了很久：同步点被推到 at(120)，拉取窗口的下限因此高过这笔成交的时间。
    harness.step(120).await;
    harness.exchange.deliver_delayed_fills();

    let canceled = harness.pause(121).await;

    // 暂停要吸收最终成交（第 9.4 节）：这笔成交比同步点早得多，但订单是那时挂出去的，
    // 所以它仍然在拉取范围内，不能漏掉。
    assert_eq!(canceled.len(), 20);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
}

/// 暂停后流水才到，而且已经超出回看窗口：恢复时要能补记。
#[tokio::test]
async fn a_fill_delivered_after_the_pause_is_still_recorded_on_resume() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 下单当时就成交了 0.02，成交记录要等到暂停之后才到。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.02").unwrap(), at(0))
        .unwrap();
    // 走到 t=120：挂单累计量解释了仓位差额，同步点推到 120。
    harness.step(120).await;
    // 暂停先撤单，再按订单状态核对：格子按订单累计量记入 .02，但流水还查不到，
    // 这一格的成交没有对完账。
    let canceled = harness.pause(120).await;
    assert_eq!(canceled.len(), 20);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap(),
        "the order's cumulative filled quantity is booked"
    );
    assert!(
        harness
            .runtime
            .fills(&harness.strategy_id)
            .unwrap()
            .is_empty(),
        "the fill record is not visible yet"
    );

    harness.exchange.deliver_delayed_fills();
    let outcome = harness.resume(121).await;
    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("expected a resumed step, got {outcome:?}");
    };

    // 流水到了：时间戳早于同步点，回查起点仍然追到它挂出去的时刻，流水照样补进表里，
    // 而且不重复记账。
    assert_eq!(report.applied_fills, 0, "the fill was already booked");
    assert_eq!(
        harness.runtime.fills(&harness.strategy_id).unwrap().len(),
        1,
        "the late fill must be recorded"
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    assert!(!report.is_paused());
}

/// 二次核对必须重新取订单快照，不能沿用查仓位之前的旧快照。
///
/// 事件序列：挂单快照里累计量是 0；查仓位之前成交 `.02`；两次补拉成交都暂时看不到
/// 这笔流水。沿用旧快照就会把「刚成交、还没入账」当成仓位对不上，误暂停一个正常
/// 运行的策略。
#[tokio::test]
async fn a_fill_hidden_from_the_fills_query_does_not_pause_the_strategy() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 成交记录查不到：第一次补拉和第二次补拉都看不到它。
    harness.exchange.delay_next_fills(1);
    // 查完挂单、查仓位之前成交 0.02：这一笔不在挂单快照里，也不在成交流水里。
    harness
        .exchange
        .fill_before_next_position(1, buy, Quantity::parse("0.02").unwrap());

    let report = harness.step(1).await;

    // 二次核对重新取挂单快照，累计量 0.02 解释了仓位差额。
    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 0, "the fill record is still hidden");
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());

    // 流水到手后照常入账。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 1);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
}

#[tokio::test]
async fn a_fully_filled_order_whose_fill_record_is_late_is_not_replaced() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 这张买单在交易所全部成交了，但成交记录要下一步才查得到。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.05").unwrap(), at(1))
        .unwrap();
    let place_count = harness.exchange.place_count();

    let report = harness.step(1).await;

    // 订单从挂单列表里消失，但交易所说它已成交：不能判成撤销、不能在同一个格子
    // 再挂一张新买单，也不能因为仓位多了一份就暂停。
    assert!(!report.is_paused());
    assert!(report.placed.is_empty());
    assert_eq!(harness.exchange.place_count(), place_count);
    assert_eq!(report.position, Position::parse("0.05").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.05").unwrap());
    assert_eq!(harness.orders().len(), 20);
    assert_eq!(harness.working().len(), 20);

    // 成交记录到了：补记进格子，订单收尾，然后照常挂出这一格的卖单。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 1);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::ZERO,
        "the whole cell was bought"
    );
    assert_eq!(harness.working().len(), 20);
    let sell = harness.working_cloid(0, OrderPurpose::GridExit);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().limit_price,
        Price::parse("96000").unwrap()
    );
}

#[tokio::test]
async fn a_fill_between_the_fills_query_and_the_position_query_does_not_pause() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 查完成交、还没查仓位时成交了 0.02。
    harness
        .exchange
        .fill_before_next_position(1, buy, Quantity::parse("0.02").unwrap());

    let report = harness.step(1).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 1);
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
}

#[tokio::test]
async fn a_fill_between_the_position_query_and_the_fills_requery_does_not_pause() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 查成交、查仓位之间成交 0.02：仓位快照里没有它，于是触发补拉成交；
    // 补拉成交之前又成交了 0.02，这一笔同样不在刚才那份仓位快照里。
    harness
        .exchange
        .fill_before_next_position(1, buy.clone(), Quantity::parse("0.02").unwrap());
    harness
        .exchange
        .fill_after_next_position(buy, Quantity::parse("0.02").unwrap());

    let report = harness.step(1).await;

    // 补拉之后重新查一次仓位：两笔都记入，仓位和预期一致，不是真的对不上。
    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 2);
    assert_eq!(report.position, Position::parse("0.04").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.04").unwrap());
    // 这一步照常往下走：买到的 0.04 挂出卖单，原来的买单还剩 0.01 在场。
    assert_eq!(report.placed.len(), 1);
    assert_eq!(
        harness.at_exchange(&report.placed[0]).unwrap().limit_price,
        Price::parse("96000").unwrap()
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.01").unwrap()
    );
}

#[tokio::test]
async fn a_cancel_of_an_order_the_exchange_never_saw_keeps_the_reservation() {
    let harness = harness().await;
    // 下单超时：交易所根本没有这张单。
    harness.exchange.unknown_next_place("timed out");
    harness.step(0).await;
    let unknown = harness
        .orders()
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Unknown)
        .expect("the unknown order must stay in the database");
    let place_count = harness.exchange.place_count();

    // 过了 expiresAfter 撤它，交易所只会回答「查无此单」：这分不出它是已成交、
    // 已撤销还是压根没落地，所以和挂单列表里消失时一样保留预留、不释放。（第一张单结果
    // 未知，这一步就没再发别的单，所以只有它一张。）
    let canceled = harness.pause(6).await;

    assert!(canceled.is_empty());
    assert_eq!(harness.exchange.place_count(), place_count);
    assert_eq!(
        harness
            .orders()
            .into_iter()
            .find(|order| order.record.client_order_id == unknown.record.client_order_id)
            .unwrap()
            .record
            .status,
        OrderStatus::Unknown
    );

    // 预留还在，恢复也不下新单：这一张的结果我们从来没拿到过，撤销只会得到「查无此单」
    // ——它可能已经成交，这时候重新挂单就会在同一格上多出一张会成交的单。状态留在
    // 「恢复中」，等期限过后挂单、成交、仓位三项核对把它作废再放行
    // （见 `unknown_orders.rs`）。
    let outcome = harness.resume(7).await;
    assert_eq!(
        outcome,
        ResumeOutcome::WaitingForSettle { working: 1 },
        "结果未知的订单不算收场"
    );
    assert_eq!(harness.exchange.place_count(), place_count, "不下任何新单");
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Restoring
    );

    // 后台每一轮继续按 `cloid` 核对，可它问不出结局：状态一直停在「恢复中」。
    let report = harness.step(8).await;
    assert_eq!(report.status, StrategyStatus::Restoring);
    assert!(report.placed.is_empty());
    assert_eq!(harness.exchange.place_count(), place_count);

    // 页面上写清楚为什么恢复不了：不是「正在重试」，是问不出结局。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let note = detail.note.unwrap_or_default();
    assert!(note.contains("查无此单"), "unexpected note: {note}");

    // 预留还在：这一格没有补挂新的买单。
    let buys: Vec<_> = harness
        .orders()
        .into_iter()
        .filter(|order| {
            order.record.unit == unknown.record.unit
                && order.record.purpose == OrderPurpose::GridEntry
        })
        .collect();
    assert_eq!(buys.len(), 1);
    assert_eq!(
        buys[0].record.client_order_id,
        unknown.record.client_order_id
    );
}

/// 部分成交之后被交易所自己撤掉（自成交保护、只减仓冲突这类终态）：
/// 成交照记进格子，剩下的预留释放出来重新挂。
#[tokio::test]
async fn a_partially_filled_order_canceled_by_the_exchange_is_booked_and_released() {
    let harness = harness().await;
    harness.step(0).await;

    // 第 0 格的买单成交 0.02（成交记录暂时查不到），随后交易所把它撤掉。
    let cloid = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&cloid, Quantity::parse("0.02").unwrap(), at(1))
        .unwrap();
    assert!(harness.exchange.cancel_externally(&cloid));

    let report = harness.step(1).await;

    // 撤销带回了最终的累计成交量：0.02 记进格子，这张单是终态。
    let order = harness
        .orders()
        .into_iter()
        .find(|order| order.record.client_order_id == cloid)
        .expect("the canceled order must stay in the database");
    assert_eq!(order.record.status, OrderStatus::Canceled);
    assert_eq!(
        order.record.cumulative_filled,
        Quantity::parse("0.02").unwrap()
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap(),
        "买到的 0.02 不再算待买回"
    );

    // 买到的 0.02 这一步就挂出卖单；补挂 0.03 的买单要等成交流水到齐：撤单只带回了
    // 累计成交量，流水还没到，这张单的成交没对完账（第 9.2 节，同
    // `a_fully_filled_order_whose_fill_record_is_late_is_not_replaced`）。
    assert_eq!(report.placed.len(), 1);
    let sell = harness.working_cloid(0, OrderPurpose::GridExit);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().quantity,
        Quantity::parse("0.02").unwrap(),
        "买到的 0.02 必须挂出卖单"
    );

    // 流水到了：这张单对完账，剩余预留释放出来补挂 0.03 的买单。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert_eq!(report.placed.len(), 1);
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    assert_eq!(
        harness.order(&buy).record.quantity,
        Quantity::parse("0.03").unwrap(),
        "剩余的 0.03 必须补挂回去"
    );
}

#[tokio::test]
async fn b18_a_position_mismatch_pauses_the_strategy_and_cancels_its_orders() {
    let harness = harness().await;
    harness.step(0).await;
    // 用户在交易所手动买入 0.01。
    harness
        .exchange
        .manual_trade(Quantity::parse("0.01").unwrap());

    let report = harness.step(1).await;

    assert!(report.is_paused());
    assert_eq!(report.position, Position::parse("0.01").unwrap());
    assert_eq!(report.expected_position, Position::ZERO);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::PositionCheckFailed)
    );
    assert!(harness.working().is_empty());
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working())
    );
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
    // 暂停之后页面不再显示等待原因：暂停原因要写在说明里，一直留到恢复。
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap_or_default();
    assert!(note.contains("仓位和应有持仓对不上"), "{note}");

    // 暂停后的一步只看失效价（这个策略没设），不下单、不撤单，仍是已暂停。
    let placed = harness.exchange.place_count();
    let report = harness.step(2).await;
    assert_eq!(report.status, StrategyStatus::Paused);
    assert!(report.placed.is_empty() && report.canceled.is_empty());
    assert_eq!(harness.exchange.place_count(), placed);

    // 仓位没修好之前，恢复会立刻再次暂停。
    let outcome = harness.resume(3).await;

    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("expected a step, got {outcome:?}");
    };
    assert!(report.is_paused());
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
}

/// 只减仓的卖单不能把仓位做到反方向。
///
/// 事件序列：格子买入 `.05` 并挂出只减仓的卖单；用户手动把仓位平掉；价格随后上穿
/// 卖价。这张卖单此刻没有任何仓位可减，不能成交。
#[tokio::test]
async fn a_reduce_only_sell_does_not_open_a_short_position() {
    let harness = harness().await;
    harness.step(0).await;
    // 跌到 95000：第 0 格买回 .05。
    harness
        .exchange
        .set_market_price(Price::parse("95000").unwrap(), at(1));
    harness.step(1).await;
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.05").unwrap()
    );
    let sell = harness.working_cloid(0, OrderPurpose::GridExit);
    assert!(harness.at_exchange(&sell).unwrap().reduce_only);

    // 用户在交易所手动清仓，运行时还没对账。
    harness
        .exchange
        .manual_trade(Quantity::parse("-0.05").unwrap());
    // 价格上穿卖价 96000。
    harness
        .exchange
        .set_market_price(Price::parse("96100").unwrap(), at(2));

    // 没有仓位可减：卖单不成交，仓位停在零，不会变成 -0.05。
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().status,
        OrderStatus::Open
    );
    // 直接指定成交也一样：只减仓的可成交数量按当前仓位收窄，收窄到零就不成交。
    assert_eq!(
        harness
            .exchange
            .fill(&sell, Quantity::parse("0.05").unwrap(), at(2))
            .unwrap(),
        Quantity::ZERO
    );
    assert_eq!(harness.exchange.current_position(), Position::ZERO);

    // 对账时发现仓位对不上：应有持仓是 .05，实际是 0，策略暂停。
    let report = harness.step(2).await;

    assert!(report.is_paused());
    assert_eq!(report.position, Position::ZERO);
    assert_eq!(report.expected_position, Position::parse("0.05").unwrap());
}

/// 写库失败时一张单都不能发出去（第 9.1 节）。
#[tokio::test]
async fn b17_nothing_is_sent_when_the_order_cannot_be_stored() {
    let harness = harness().await;
    // 让写订单表失败，模拟磁盘写不进去。
    harness
        .runtime
        .store()
        .fail_order_writes_for_test()
        .unwrap();

    harness.clock.set(at(0));
    let error = harness.runtime.step(&harness.strategy_id).await;

    assert!(matches!(error, Err(RuntimeError::Store(_))));
    // 先写库、后发送：写不进去就一张单也不发。
    assert_eq!(harness.exchange.place_count(), 0);
    assert!(harness.orders().is_empty());
}

#[tokio::test]
async fn b23_an_invalid_config_is_refused_before_anything_is_stored() {
    let console = common::console();
    let mut config = long_grid_config();
    // 价差 1 美元覆盖不了两条腿的 taker 费率。
    long_side_mut(&mut config).grid_spread = poise_strategy::units::Price::parse("1").unwrap();

    let draft_id = console.runtime.create_draft(config).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    // 预览就说不过去：只有原因，没有表格，也没有确认令牌。
    assert!(!preview.validation.valid);
    assert!(preview.validation.reason.is_some());
    assert!(preview.token.is_none());
    assert!(preview.batches.is_empty());
    // 没拿到令牌也就启动不了，一张策略记录都不会留下。
    assert!(matches!(
        console.runtime.start_draft(&draft_id, "whatever").await,
        Err(RuntimeError::Config(_))
    ));
    assert!(console.runtime.strategies().unwrap().is_empty());

    // 数据库仍然可用：同一批参数换一个合法的价差就能启动。
    let mut valid = long_grid_config();
    long_side_mut(&mut valid).grid_spread = poise_strategy::units::Price::parse("1000").unwrap();
    let strategy_id = console.start(valid).await;
    assert_eq!(
        console.runtime.status(&strategy_id).unwrap(),
        StrategyStatus::Running
    );
}

#[tokio::test]
async fn step_on_an_unknown_strategy_is_refused() {
    let harness = harness().await;
    let missing = poise_runtime::StrategyId::try_from("s-missing".to_string()).unwrap();

    let error = harness.runtime.step(&missing).await.unwrap_err();

    assert!(matches!(
        error,
        RuntimeError::Store(poise_runtime::StoreError::UnknownStrategy(_))
    ));
}

#[test]
fn the_runtime_can_be_built_from_any_exchange_implementation() {
    // 只验证 trait 的形状：模拟交易所可以作为 `Arc<dyn Exchange>` 传进去。
    let exchange: std::sync::Arc<dyn poise_runtime::Exchange> = common::exchange();
    let account: std::sync::Arc<dyn poise_runtime::Account> = common::account();
    let runtime = Runtime::new(exchange, account, Store::open_in_memory().unwrap());

    assert_eq!(runtime.options().order_expires_after.num_seconds(), 5);
    assert_eq!(
        SimulatedExchange::new(
            btc(),
            poise_strategy::units::Price::parse("1").unwrap(),
            at(0)
        )
        .current_position(),
        Position::ZERO
    );
}
