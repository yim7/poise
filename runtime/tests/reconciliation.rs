//! 对账（第 9.2、9.3 节）。
//!
//! 订单的累计成交量以交易所的订单状态（挂单列表、按 `cloid` 查询）为准，成交流水只是
//! 审计记录，不挡记账。撤单之后、交易所给出最终累计量之前，这个单位不下同用途的新单：
//! 按撤单前的数量补一张就会多买或者多卖。拉流水的起点不被挂了很久的单钉住。这里还
//! 验证仓位数据还在变的时候不误暂停，以及成功路径上的「先写库、再发送」。

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
    let harness = core_harness().await;
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

/// 成交流水不参与记账：订单状态给出的累计成交量就是权威数字（第 9.3 节）。
///
/// 事件序列：95 格的买单 `.05` 在交易所全部成交，成交流水迟迟查不到；暂停时按 `cloid`
/// 查到 `Filled/.05`，直接按订单状态把 `.05` 记进格子，这张单不用再撤，暂停当场收尾。
/// 恢复后这一格挂出 `.05` 的卖单，不会再买一份，也不等流水。流水后来到了只进流水表，
/// 不重复记账。
#[tokio::test]
async fn a_filled_order_is_booked_from_its_status_without_waiting_for_the_fill_records() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.05").unwrap(), at(1))
        .unwrap();

    let canceled = harness.pause(1).await;

    assert!(!canceled.contains(&buy), "the filled order needs no cancel");
    assert_eq!(canceled.len(), 19);
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused,
        "the pause settles without the fill records"
    );
    assert_eq!(harness.order(&buy).record.status, OrderStatus::Filled);
    assert_eq!(harness.cell(0).to_buy_back, Quantity::ZERO);

    let outcome = harness.resume(2).await;
    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("the resume must go through: {outcome:?}");
    };
    assert_eq!(report.position, Position::parse("0.05").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.05").unwrap());
    assert!(!report.plan.waiting.contains(&WaitReason::FillsPending));
    assert!(
        !report.plan.place.iter().any(|intent| {
            intent.unit == long_cell(0) && intent.purpose == OrderPurpose::GridEntry
        }),
        "no second buy for the same cell"
    );
    let sell = harness.working_cloid(0, OrderPurpose::GridExit);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().quantity,
        Quantity::parse("0.05").unwrap()
    );

    // 流水到手：只写进流水表，不再记一遍。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(3).await;

    assert_eq!(report.applied_fills, 0, "the fill was already booked");
    assert_eq!(harness.cell(0).to_buy_back, Quantity::ZERO);
    let fills = harness.runtime.fills(&harness.strategy_id).unwrap();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].client_order_id, buy);
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.05").unwrap()
    );
}

/// 还在场的单部分成交、流水一直查不到：按挂单列表报的累计量记账，这一格照常往下走。
///
/// 事件序列：95 格的买单 `.05` 在 `t = 1` 成交 `.02`，这笔流水再也查不到。下一步按挂单
/// 列表里的累计量把 `.02` 记进格子，挂出 `.02` 的卖单，剩下 `.03` 的买单留在场上。
/// 迟迟不到的流水只是审计记录，不让这一格停在「成交待对账」上。
#[tokio::test]
async fn a_partial_fill_of_a_working_order_is_booked_from_the_working_list() {
    let harness = harness().await;
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.02").unwrap(), at(1))
        .unwrap();

    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert!(!report.plan.waiting.contains(&WaitReason::FillsPending));
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
    let order = harness.order(&buy);
    assert_eq!(
        order.record.cumulative_filled,
        Quantity::parse("0.02").unwrap()
    );
    assert!(
        order.record.is_working(),
        "the rest of the buy stays working"
    );
    assert!(order.fills_reconciled);
    let sell = harness.working_cloid(0, OrderPurpose::GridExit);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().quantity,
        Quantity::parse("0.02").unwrap()
    );

    // 很久之后流水仍然没有：这一格照常，流水表里没有这一笔。
    let report = harness.step(600).await;
    assert!(!report.is_paused());
    assert!(!report.plan.waiting.contains(&WaitReason::FillsPending));
    assert!(
        harness
            .runtime
            .fills(&harness.strategy_id)
            .unwrap()
            .is_empty()
    );
}

/// 挂了很久的单不再把拉流水的起点钉在它挂出的时刻（第 9.3 节）。
///
/// 起点是上一次同步点往前 60 秒，最多往前 1 小时：Hyperliquid 的成交接口按整个账户返回、
/// 只留最近一万笔，从几天前查起会越查越多，最后永远查不全。
#[tokio::test]
async fn a_long_resting_order_does_not_pin_the_fill_lookback() {
    let harness = harness().await;
    harness.step(0).await;
    assert_eq!(harness.working().len(), 20, "twenty buys rest from t = 0");

    // 停了很久之后的第一步：同步点还在 t = 0，起点最多往前 1 小时。
    harness.step(100_000).await;
    assert_eq!(
        harness.exchange.last_fills_since(),
        Some(at(100_000 - 3_600))
    );

    // 再下一步：从上一次同步点往前 60 秒。买单都还挂着，起点不回到 t = 0。
    harness.step(100_005).await;
    assert_eq!(harness.exchange.last_fills_since(), Some(at(100_000 - 60)));
    assert_eq!(harness.working().len(), 20);
}

/// 成交流水比订单累计成交量还多：数据本身有问题，暂停交给用户处理。
///
/// 事件序列：`.05` 的买单成交 `.02`，流水已保存；交易所又把同一笔成交换了个成交编号
/// 报告一遍。正常数据不会这样——每笔成交的累计量已经包含它之前的全部成交，所以一张
/// 订单全部流水之和最多等于它最终的累计成交量。多出来的记录说明成交被记到了错的订单上，
/// 继续自动运行没有意义：撤单、停在「已暂停」，暂停原因一直显示到用户恢复。
#[tokio::test]
async fn fill_records_that_exceed_the_cumulative_pause_the_strategy() {
    let harness = harness().await;
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
    // 订单按交易所的累计量对完了账，撤单收得了尾：落在「已暂停」，不会再下任何新单，
    // 页面上看得到原因，等用户处理。
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
    assert!(harness.working().is_empty());
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap_or_default();
    assert!(note.contains("成交记录"), "{note}");
    let report = harness.step(3).await;
    assert!(report.placed.is_empty());
    assert_eq!(report.status, StrategyStatus::Paused);
}

/// 连续两次成交之后仓位数据还在变：这一步不下新单，也不暂停。
///
/// 事件序列：两次查仓位之间各成交一笔 `.01`，两笔流水都暂时查不到。第一次核对读到
/// `.01`、第二次读到 `.02`，两次读到的数据不一样——数据还在变，说明不了仓位有问题。
/// 有限轮之内稳定下来就照常往下走；只有连续两轮读到同一份数据、而且仍然解释不了，
/// 才是真的仓位异常。每一轮按挂单列表的累计量记账，不等流水。
#[tokio::test]
async fn a_position_that_settles_within_the_check_rounds_does_not_pause_the_strategy() {
    let harness = harness().await;
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
    // 两轮核对各从挂单列表记一笔。
    assert_eq!(report.applied_fills, 2);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
    // 最后一轮读到的是 .02：记账追上了仓位，数据稳定下来。
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    // 买到的 .02 挂出卖单。
    assert_eq!(report.placed.len(), 1);
    assert_eq!(
        harness.at_exchange(&report.placed[0]).unwrap().limit_price,
        Price::parse("96000").unwrap()
    );

    // 流水到手：只进流水表，不重复记账。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 0);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
    assert_eq!(report.position, report.expected_position);
    assert!(report.placed.is_empty());
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
    let harness = harness().await;
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
    assert!(
        report.placed.is_empty(),
        "no new orders while the data moves"
    );
    assert!(report.plan.waiting.contains(&WaitReason::PositionUnstable));

    // 下一步数据稳定：仓位和记账一致。流水到手也不会重复记账。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(3).await;

    assert!(!report.is_paused());
    assert_eq!(report.position, report.expected_position);
    assert_eq!(
        Position::from_quantity(
            Quantity::parse("0.05")
                .unwrap()
                .saturating_sub(harness.cell(0).to_buy_back)
        ),
        report.position,
        "the cell holds exactly what the exchange holds"
    );
}

/// 核对轮次用完时数据还在变：不下新单，记等待原因，下一步再核对。
#[tokio::test]
async fn a_position_that_keeps_changing_waits_instead_of_pausing() {
    let harness = harness().await;
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
    // 三轮核对各从挂单列表记一笔，第四笔在最后一次查仓位之前才成交。
    assert_eq!(report.applied_fills, 3);
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

    // 下一步把第四笔记上（流水先到，按它的累计量一次记齐），仓位和数据一致。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 1);
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
    let harness = harness().await;

    let report = harness.step(0).await;

    // 交易所每次收到下单请求时都查了一次库，查到的就是这一步下的那些单。
    let probes = harness.exchange.stored_before_place();
    let placed: Vec<_> = probes.iter().map(|(cloid, _)| cloid.clone()).collect();
    assert_eq!(placed, report.placed);
    assert_eq!(placed.len(), 20);
    assert!(probes.iter().all(|(_, stored)| *stored));
}
