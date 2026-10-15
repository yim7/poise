//! 成交核对没完成时不下同用途的新单（第 9.2 节）。
//!
//! 一条规则覆盖三种事件序列：撤单期间的部分成交、恢复时还没到的流水、超出回看窗口
//! 的迟到流水。它们的共同点是订单的成交还没对完账，这时按撤单前的数量补一张单就会
//! 多买或者多卖。这里还验证仓位数据还在变的时候不误暂停，以及成功路径上的
//! 「先写库、再发送」。

mod common;

use common::{at, core_harness, harness, long_cell};
use poise_runtime::{ResumeOutcome, StrategyStatus};
use poise_strategy::order::{OrderPurpose, OrderStatus};
use poise_strategy::planning::WaitReason;
use poise_strategy::units::{Position, Price, Quantity};

/// 撤单期间成交了一部分时，替换单要等对完账再按新的数量下。
///
/// 事件序列：95 这一格卖出一部分之后要买回，买回单挂在自己的买价 `95000`，把 94 格的
/// 卖单顶到 `95001`；买回成交后这张买单不再在场，94 格的卖单要改回 `95000`——
/// 撤单期间它成交了 `.005`。这时按撤单前的数量补一张 `.015` 的卖单，就超过了这一格
/// 实际持有的 `.010`：多出来的部分会卖到主仓（只减仓是按整个账户算的，拦不住它）。
#[tokio::test]
async fn a_sell_filled_while_being_canceled_is_not_replaced_with_the_old_quantity() {
    let harness = core_harness();
    // 从 94500 开始：95 批的主仓买单挂在自己的批次价 95000，当场成交。
    harness
        .exchange
        .set_market_price(Price::parse("94500").unwrap(), at(0));
    harness.step(0).await;
    assert_eq!(
        harness.core_filled(0),
        Quantity::parse("0.175").unwrap(),
        "the core buy at 95000 is marketable"
    );

    // 95 这一格启用并当场买入（它的买价就是 95000），94 格挂着买单等价格下来。
    harness.step(1).await;
    assert_eq!(harness.cell(0).to_buy_back, Quantity::ZERO);

    // 买到的 .015 挂出卖单。
    let report = harness.step(2).await;
    assert_eq!(report.placed.len(), 1);
    assert_eq!(
        harness.at_exchange(&report.placed[0]).unwrap().limit_price,
        Price::parse("96000").unwrap()
    );

    // 卖单成交 .005（这一格要买回 .005），同时 94 格的买单成交整份。
    let sell = harness.working_cloid(0, OrderPurpose::GridExit);
    harness
        .exchange
        .fill(&sell, Quantity::parse("0.005").unwrap(), at(3))
        .unwrap();
    let buy = harness.working_cloid(1, OrderPurpose::GridEntry);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.015").unwrap(), at(3))
        .unwrap();
    let report = harness.step(3).await;

    // 买回单挂在自己的买价 95000 上，把 94 格的卖单顶到 95001。
    assert_eq!(report.placed.len(), 2);
    let sell = harness.working_cloid(1, OrderPurpose::GridExit);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().limit_price,
        Price::parse("95001").unwrap(),
        "the buy-back order pushes the sell up by one tick"
    );

    // 买回成交，这张买单不再在场：94 格的卖单要改回 95000。
    // 撤单期间它又成交了 .005。
    harness.exchange.fill_before_next_cancel(
        1,
        sell.clone(),
        Quantity::parse("0.005").unwrap(),
        at(4),
    );
    let report = harness.step(4).await;

    // 旧卖单撤掉了，替换单要等它的成交对完账：这一步不为 94 格挂卖单。
    assert_eq!(report.canceled, vec![sell.clone()]);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().status,
        OrderStatus::Canceled
    );
    assert!(
        !report.plan.place.iter().any(|intent| {
            intent.unit == long_cell(1) && intent.purpose == OrderPurpose::GridExit
        }),
        "the replacement must wait until the canceled order's fills are reconciled"
    );
    assert!(report.plan.waiting.contains(&WaitReason::FillsPending));
    // 其余的规划照常执行：95 格卖单的剩余量不够，补一张 .005。
    assert_eq!(report.placed.len(), 1);
    assert_eq!(
        harness.at_exchange(&report.placed[0]).unwrap().quantity,
        Quantity::parse("0.005").unwrap()
    );

    // 成交对完账之后，按实际持有的 .010 挂卖单，而不是撤单前的 .015。
    let report = harness.step(5).await;

    assert_eq!(report.applied_fills, 1);
    assert_eq!(
        harness.cell(1).to_buy_back,
        Quantity::parse("0.005").unwrap()
    );
    let sell = harness.working_cloid(1, OrderPurpose::GridExit);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().quantity,
        Quantity::parse("0.010").unwrap(),
        "the replacement uses the quantity left after the partial fill"
    );
    assert_eq!(report.position, report.expected_position);
}

/// 恢复时成交流水还没到：完全成交的旧单不能失去预留，否则会重复买入。
///
/// 事件序列：95 格的买单 `.05` 在交易所全部成交，但成交流水迟迟查不到；策略被暂停，
/// 撤单返回成功，本地因为累计量还是零把它记成「已撤销」；不交付流水直接恢复。
/// 订单查询返回 `Filled/.05`，成交核对没有完成——这时如果只为「还在场」的订单保留
/// 数量，这一格的预留就没了，会再买一份 `.05`，持仓变成 `.10`。
#[tokio::test]
async fn a_fully_filled_order_keeps_its_reservation_while_its_fills_are_missing() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 整份成交，但成交流水查不到。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.05").unwrap(), at(1))
        .unwrap();

    // 暂停：撤单只把它变成终态，成交还没核对。
    let canceled = harness.pause(1).await;
    assert!(canceled.contains(&buy));
    assert_eq!(canceled.len(), 20, "every working order is canceled");
    assert_eq!(
        harness
            .orders()
            .into_iter()
            .find(|order| order.record.client_order_id == buy)
            .unwrap()
            .record
            .status,
        OrderStatus::Canceled
    );

    // 不交付流水就恢复。
    let outcome = harness.resume(2).await;
    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("the canceled order must not block the resume: {outcome:?}");
    };

    // 交易所上的 .05 由订单的累计成交量解释，仓位核对通过。
    assert_eq!(report.position, Position::parse("0.05").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.05").unwrap());
    // 但这一格不能再买：它的预留还在，只是成交没核对完。
    assert!(
        !report.plan.place.iter().any(|intent| {
            intent.unit == long_cell(0) && intent.purpose == OrderPurpose::GridEntry
        }),
        "a fully filled order must not lose its reservation"
    );
    assert!(report.plan.waiting.contains(&WaitReason::FillsPending));
    assert_eq!(
        harness
            .orders()
            .into_iter()
            .filter(|order| order.record.unit == long_cell(0)
                && order.record.purpose == OrderPurpose::GridEntry)
            .count(),
        1,
        "no second buy for the same cell"
    );

    // 流水到手：整份成交记入，这一格挂出卖单而不是再买一份。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(3).await;

    assert_eq!(report.applied_fills, 1);
    assert_eq!(harness.cell(0).to_buy_back, Quantity::ZERO);
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.05").unwrap()
    );
    let sell = harness.working_cloid(0, OrderPurpose::GridExit);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().quantity,
        Quantity::parse("0.05").unwrap()
    );
}

/// 超出回看窗口的迟到流水最终还是要写进流水表。
///
/// 事件序列：95 格的买单在 `t = 0` 成交 `.02`，流水一直查不到；`t = 120` 暂停，
/// `t = 121` 恢复时流水仍然查不到。恢复时按订单状态把 `.02` 补记进格子——
/// 这一步同时要把「成交还没对完账」记下来，否则回查起点会跟着同步点前移到
/// `t = 61`，时间戳是 `t = 0` 的流水就永远读不到了。
#[tokio::test]
async fn a_late_fill_outside_the_lookback_window_still_lands_in_the_fills_table() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.02").unwrap(), at(0))
        .unwrap();

    // 走到回看窗口（60 秒）之外，暂停时流水仍然查不到。
    let report = harness.step(120).await;
    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 0);
    assert!(
        harness
            .runtime
            .fills(&harness.strategy_id)
            .unwrap()
            .is_empty()
    );
    let canceled = harness.pause(120).await;
    assert!(canceled.contains(&buy));

    // 恢复：按订单状态补记 .02，但流水还没到，这一格仍然不下买单。
    let outcome = harness.resume(121).await;
    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("the canceled order must not block the resume: {outcome:?}");
    };
    assert_eq!(report.applied_fills, 1);
    assert!(
        harness
            .runtime
            .fills(&harness.strategy_id)
            .unwrap()
            .is_empty(),
        "the fill record is still not visible"
    );
    // 成交还没对完账：这一格的预留还在，不能按订单状态补单。
    assert!(report.plan.waiting.contains(&WaitReason::FillsPending));
    assert!(!report.plan.place.iter().any(|intent| {
        intent.unit == long_cell(0) && intent.purpose == OrderPurpose::GridEntry
    }));

    // 流水终于到手：虽然时间戳早于同步点，仍然写进流水表，并且不重复记账。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(122).await;

    assert_eq!(report.applied_fills, 0, "the fill was already booked");
    let fills = harness.runtime.fills(&harness.strategy_id).unwrap();
    assert_eq!(fills.len(), 1, "the late fill must be recorded");
    assert_eq!(fills[0].client_order_id, buy);
    assert_eq!(fills[0].quantity, Quantity::parse("0.02").unwrap());
    // 这一格的成交对完账了，下一步照常规划。
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
}

/// 记入新成交会让「已对账」失效，否则满成交之后早期流水会永久漏存。
///
/// 事件序列：`.05` 的买单先走一次没有成交的 `step`，被标为已对账。`t = 2` 先成交 `.02`
/// 并隐藏这笔流水，再成交 `.03`，末笔流水可见、携带累计量 `.05`。下一次补拉按累计量
/// 记入 `.05`，把订单变成 `Filled`——如果沿用原来的「已对账」，这张单从此不再被核对，
/// 回查起点跟着同步点前移，`t = 2` 的 `.02` 就永远读不到了。
#[tokio::test]
async fn a_booked_fill_puts_an_already_reconciled_order_back_to_pending() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);

    // 没有成交的一步：这张单的成交对完账了。
    harness.step(1).await;
    assert!(
        harness.order(&buy).fills_reconciled,
        "an order with nothing to reconcile is marked as reconciled"
    );

    // t = 2：先成交 .02，这笔流水暂时查不到；再成交 .03，末笔流水可见、累计量 .05。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .inject_fill(
            "trade-a",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(2),
        )
        .unwrap();
    harness
        .exchange
        .inject_fill(
            "trade-b",
            &buy,
            Quantity::parse("0.03").unwrap(),
            Quantity::parse("0.05").unwrap(),
            at(2),
        )
        .unwrap();

    let report = harness.step(2).await;

    assert_eq!(report.applied_fills, 1, "the visible fill books .05");
    let order = harness.order(&buy);
    assert_eq!(order.record.status, OrderStatus::Filled);
    assert_eq!(
        order.record.cumulative_filled,
        Quantity::parse("0.05").unwrap()
    );
    // 记入新成交之后不能再算「已对账」：流水表里只有 .03，还差 .02。
    assert!(
        !order.fills_reconciled,
        "booking a fill invalidates the reconciled mark"
    );

    // 走到回看窗口（60 秒）之外，缺的那笔流水仍然查不到。
    let report = harness.step(120).await;
    assert!(!report.is_paused());
    assert!(
        !harness
            .runtime
            .fills(&harness.strategy_id)
            .unwrap()
            .iter()
            .any(|fill| fill.trade_id == "trade-a")
    );

    // 流水终于到手：时间戳 t = 2 已经早于同步点，仍然写进流水表。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(122).await;

    assert!(!report.is_paused());
    let fills = harness.runtime.fills(&harness.strategy_id).unwrap();
    assert!(
        fills
            .iter()
            .any(|fill| fill.trade_id == "trade-a"
                && fill.quantity == Quantity::parse("0.02").unwrap()),
        "the fill hidden before the order was marked reconciled must still be fetched"
    );
    // 流水补齐了，这张单这才算对完账。
    assert!(harness.order(&buy).fills_reconciled);
}

/// 重复报告不能填补真正缺失的成交。
///
/// 事件序列：`.05` 的买单成交 `.02`，流水已保存；交易所又把同一笔成交重新报告了一遍
/// （成交编号相同，这才是真实情况）；接着真实成交 `.01`（累计 `.03`），这笔流水
/// 暂时查不到；再成交 `.02`（累计 `.05`），流水可见。本地和交易所的累计量都是 `.05`，
/// 但流水表里只有 `.02 + .02 = .04`：按「不少于」判定会认定已经对账，真实缺失的
/// `.01` 从此不再补拉，回查起点也跟着同步点前移。
///
/// 补齐之后这张单要解除 pending，这一格按状态恢复正常规划——否则它会永久占着
/// 预留，这一格卖出之后再挂不出买回单。
#[tokio::test]
async fn a_duplicate_report_does_not_cover_a_missing_fill() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);

    // 真实成交 .02，流水已保存。
    harness
        .exchange
        .inject_fill(
            "trade-a",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(1),
        )
        .unwrap();
    harness.step(1).await;

    // 交易所把同一笔成交重新报告一遍；接着真实成交 .01（累计 .03）暂时查不到，
    // 再成交 .02（累计 .05）可见。
    harness
        .exchange
        .inject_fill(
            "trade-a",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(1),
        )
        .unwrap();
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .inject_fill(
            "trade-b",
            &buy,
            Quantity::parse("0.01").unwrap(),
            Quantity::parse("0.03").unwrap(),
            at(2),
        )
        .unwrap();
    harness
        .exchange
        .inject_fill(
            "trade-c",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.05").unwrap(),
            at(2),
        )
        .unwrap();

    harness.step(2).await;

    let order = harness.order(&buy);
    assert_eq!(
        order.record.cumulative_filled,
        Quantity::parse("0.05").unwrap()
    );
    assert_eq!(order.record.status, OrderStatus::Filled);
    // 流水表里是 .04，比记入的 .05 少：重复报告不能填补真正缺失的那笔。
    assert!(
        !order.fills_reconciled,
        "a duplicate report must not cover the missing fill"
    );

    // 走到回看窗口之外，缺的那笔流水仍然查不到。
    let report = harness.step(120).await;
    assert!(!report.is_paused());
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(122).await;

    let fills = harness.runtime.fills(&harness.strategy_id).unwrap();
    assert!(
        fills
            .iter()
            .any(|fill| fill.trade_id == "trade-b"
                && fill.quantity == Quantity::parse("0.01").unwrap()),
        "the missing fill must still be fetched"
    );
    // 流水补齐：这张单解除 pending，这一格恢复正常规划（买单全部成交，卖出 .05）。
    assert!(harness.order(&buy).fills_reconciled);
    assert!(!report.plan.waiting.contains(&WaitReason::FillsPending));
    let sells: Vec<_> = harness
        .working()
        .into_iter()
        .filter(|order| {
            order.record.unit == long_cell(0) && order.record.purpose == OrderPurpose::GridExit
        })
        .collect();
    let sell_total = sells.iter().fold(Quantity::ZERO, |total, order| {
        total.saturating_add(order.record.quantity)
    });
    assert_eq!(sell_total, Quantity::parse("0.05").unwrap());

    // 卖单成交之后这一格要买回：pending 已经解除，买回单照常挂出。
    for sell in &sells {
        harness
            .exchange
            .fill(&sell.record.client_order_id, sell.record.quantity, at(123))
            .unwrap();
    }
    let report = harness.step(123).await;

    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.05").unwrap()
    );
    assert!(
        report.plan.place.iter().any(|intent| {
            intent.unit == long_cell(0) && intent.purpose == OrderPurpose::GridEntry
        }),
        "the cell must be able to buy back once the order is reconciled"
    );
}

/// 成交流水比订单累计成交量还多：数据本身有问题，暂停交给用户处理。
///
/// 事件序列：`.05` 的买单成交 `.02`，流水已保存；交易所又把同一笔成交换了个成交编号
/// 报告一遍。正常数据不会这样——每笔成交的累计量已经包含它之前的全部成交，所以一张
/// 订单全部流水之和最多等于它最终的累计成交量。多出来的这条记录永远对不平，
/// 订单会一直占着 pending、这一格再也挂不出新单，所以不能让它无声地卡住。
#[tokio::test]
async fn fill_records_that_exceed_the_cumulative_pause_the_strategy() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);

    // 真实成交 .02，流水已保存：这一步两边一致，照常运行。
    harness
        .exchange
        .inject_fill(
            "trade-a",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(1),
        )
        .unwrap();
    let report = harness.step(1).await;
    assert!(!report.is_paused(), "the first report is consistent");

    // 同一笔成交换了成交编号再报一次：流水表里多出一条 .02，订单累计量还是 .02。
    harness
        .exchange
        .inject_fill(
            "trade-a-again",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(1),
        )
        .unwrap();
    let report = harness.step(2).await;

    assert!(
        report.is_paused(),
        "inconsistent fill records must stop the strategy"
    );
    assert_eq!(report.status, StrategyStatus::Paused);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::FillRecordsInconsistent),
        "{report:?}"
    );
    assert!(
        report.placed.is_empty(),
        "no new orders on inconsistent data"
    );
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
}

/// 连续两次成交之后仓位数据还在变：这一步不下新单，也不暂停。
///
/// 事件序列：两次查仓位之间各成交一笔 `.01`，两笔流水都暂时查不到。第一次核对读到
/// `.01`、第二次读到 `.02`，两次读到的数据不一样——数据还在变，说明不了仓位有问题。
/// 有限轮之内稳定下来就照常往下走；只有连续两轮读到同一份数据、而且仍然解释不了，
/// 才是真的仓位异常。
#[tokio::test]
async fn a_position_that_settles_within_the_check_rounds_does_not_pause_the_strategy() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 两笔成交都查不到，并且分别落在两次查仓位之前。
    harness.exchange.delay_next_fills(2);
    harness
        .exchange
        .fill_before_next_position(2, buy, Quantity::parse("0.01").unwrap());

    let report = harness.step(1).await;

    assert!(!report.is_paused());
    assert_eq!(report.status, StrategyStatus::Running);
    assert_eq!(report.applied_fills, 0, "both fill records are hidden");
    // 最后一轮读到的是 .02：订单快照追上了仓位，数据稳定下来。
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    // 成交还没入账，这一步没有新单要下。
    assert!(report.placed.is_empty());

    // 流水到手后照常入账，两笔都记上。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 2);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    // 买到的 .02 挂出卖单。
    assert_eq!(report.placed.len(), 1);
    assert_eq!(
        harness.at_exchange(&report.placed[0]).unwrap().limit_price,
        Price::parse("96000").unwrap()
    );
}

/// 一买一卖相抵时净仓位不变，但订单快照在变：不能判为数据稳定，更不能暂停。
///
/// 事件序列：这一格持有 `.02`，买单累计 `.02`、剩余 `.03`，同时挂着 `.02` 的卖单。
/// 三笔流水都暂时查不到：
/// - 第一轮订单查询之后、第一次查仓位之前，买入 `.01`；
/// - 第一轮核对刷新订单之前，卖出 `.01`；
/// - 第一轮刷新之后、第二次查仓位之前，再买入 `.01`。
///
/// 两轮读到的净仓位都是 `.03`、预计仓位都是 `.02`（买单多出的 `.01` 和卖单多出的
/// `.01` 正好相抵），只看这两项就会判成「连续两轮读到同一份数据」，于是暂停一个
/// 正常运行的策略。订单快照其实从「买单 .02、卖单 0」变成了「买单 .03、卖单 .01」，
/// 数据明显还在动。
#[tokio::test]
async fn alternating_buys_and_sells_do_not_look_like_stable_data() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 买入 .02：待买回剩 .03，这一格挂出 .02 的卖单。
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.02").unwrap(), at(1))
        .unwrap();
    harness.step(1).await;
    let sell = harness.working_cloid(0, OrderPurpose::GridExit);

    // 四次查仓位之前各安排一笔；第一轮刷新订单之前再卖出一笔。
    harness.exchange.delay_next_fills(5);
    harness
        .exchange
        .fill_before_next_position(3, buy, Quantity::parse("0.01").unwrap());
    harness
        .exchange
        .fill_before_next_position(1, sell.clone(), Quantity::parse("0.01").unwrap());
    harness
        .exchange
        .fill_after_next_position(sell, Quantity::parse("0.01").unwrap());

    let report = harness.step(2).await;

    assert!(
        !report.is_paused(),
        "net position standing still is not stable data while the orders keep moving"
    );
    assert_eq!(report.status, StrategyStatus::Running);
    assert_eq!(report.applied_fills, 0, "every fill record is hidden");
    assert!(
        report.placed.is_empty(),
        "no new orders while the data moves"
    );
    assert!(report.plan.waiting.contains(&WaitReason::PositionUnstable));

    // 流水到手后照常入账，仓位和数据一致。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(3).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 5);
    assert_eq!(report.position, report.expected_position);
}

/// 核对轮次用完时数据还在变：不下新单，记等待原因，下一步再核对。
#[tokio::test]
async fn a_position_that_keeps_changing_waits_instead_of_pausing() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 每一轮核对之前都成交一笔 `.01`，三次核对读到的数据各不相同。
    harness.exchange.delay_next_fills(4);
    harness
        .exchange
        .fill_before_next_position(4, buy, Quantity::parse("0.01").unwrap());

    let report = harness.step(1).await;

    assert!(!report.is_paused());
    assert_eq!(report.status, StrategyStatus::Running);
    assert_eq!(report.applied_fills, 0, "every fill record is hidden");
    // 最后一次读到的仓位 .04 还没有被订单快照解释：数据不稳定，不是仓位异常。
    assert_eq!(report.position, Position::parse("0.04").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.03").unwrap());
    assert!(
        report.placed.is_empty(),
        "no new orders while the data moves"
    );
    assert!(report.canceled.is_empty());
    assert!(report.plan.waiting.contains(&WaitReason::PositionUnstable));
    // 提前返回的报告里标记价也要是真的，不能是零值。
    assert_eq!(
        report.plan.market_price,
        Price::parse(common::INITIAL_MARKET_PRICE).unwrap()
    );

    // 流水到手后照常入账，仓位和数据一致。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 4);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.01").unwrap()
    );
    assert_eq!(report.position, Position::parse("0.04").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.04").unwrap());
}

/// 成功路径也要先写库再发送：下单请求到达交易所时，这张单已经在库里。
#[tokio::test]
async fn every_placement_is_stored_before_it_is_sent() {
    let harness = harness();

    let report = harness.step(0).await;

    // 交易所每次收到下单请求时都查了一次库，查到的就是这一步下的那些单。
    let probes = harness.exchange.stored_before_place();
    let placed: Vec<_> = probes.iter().map(|(cloid, _)| cloid.clone()).collect();
    assert_eq!(placed, report.placed);
    assert_eq!(placed.len(), 20);
    assert!(probes.iter().all(|(_, stored)| *stored));
}
