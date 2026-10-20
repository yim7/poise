//! 策略生命周期里网页会点的那些动作：暂停、恢复、停止归档（第 2.4 节，B45）。
//!
//! 恢复分两条路：网页上的「恢复」只把暂停的策略接着跑起来；进程重启后的恢复
//! 走 `recover`，它必须撤掉遗留挂单——那些单的结局数据库里无从得知。

mod common;

use chrono::Duration;
use common::{at, harness, throttled};
use poise_runtime::{ResumeOutcome, RuntimeError, StrategyStatus};
use poise_strategy::order::OrderPurpose;
use poise_strategy::units::{Position, Price, Quantity};

/// B45：停止撤销全部挂单、保留仓位、策略归档只读。
#[tokio::test]
async fn b45_stopping_cancels_every_order_and_archives_the_strategy() {
    let harness = harness().await;
    harness.step(0).await;
    assert_eq!(harness.working().len(), 20);

    // 一格买回一半，制造一个仓位：停止不该动它。
    let cell0_buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness
        .exchange
        .fill(&cell0_buy, Quantity::parse("0.02").unwrap(), at(10))
        .unwrap();
    harness.step(11).await;
    let position = harness.exchange.current_position();
    assert_eq!(position, Position::parse("0.02").unwrap());

    let working_before = harness.working().len();
    let canceled = harness.stop(12).await;
    assert_eq!(
        canceled.len(),
        working_before,
        "撤掉的是停止那一刻还在场的全部订单"
    );
    assert!(harness.working().is_empty());
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopped
    );
    // 仓位保留。
    assert_eq!(harness.exchange.current_position(), position);

    // 归档只读：不能再驱动、不能再改参数、不能再暂停或恢复。
    let error = harness
        .runtime
        .step(&harness.strategy_id)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Archived(_)),
        "step: {error:?}"
    );
    let error = harness
        .runtime
        .update_config(
            &harness.strategy_id,
            throttled(harness.config(), Duration::hours(1)),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Archived(_)),
        "update_config: {error:?}"
    );
    let error = harness
        .runtime
        .pause(&harness.strategy_id)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Archived(_)),
        "pause: {error:?}"
    );
    let error = harness
        .runtime
        .resume(&harness.strategy_id)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Archived(_)),
        "resume: {error:?}"
    );
    // 再停一次不报错：结果一样。
    assert!(harness.stop(13).await.is_empty());

    // 只读的东西还在：订单记录、参数版本、状态都能看。
    assert_eq!(harness.orders().len(), working_before);
    assert!(
        !harness
            .runtime
            .config_versions(&harness.strategy_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
}

/// B45：还有订单撤不掉时停在「停止中」，常驻服务接着撤，撤干净才归档。
///
/// 归档之后没有人再驱动这个策略，留在交易所上的单就永远留在那里了。所以撤不干净时
/// 不报错也不归档：状态停在「停止中」，网页上看得到，后台每一轮继续撤。
#[tokio::test]
async fn b45_a_cancel_that_cannot_be_confirmed_keeps_the_strategy_stopping() {
    let harness = harness().await;
    harness.step(0).await;
    // 有一张单的撤单请求超时：交易所那边它还在。
    harness.exchange.unknown_next_cancel("timed out");

    let canceled = harness.stop(1).await;

    assert_eq!(canceled.len(), 19, "撤不掉的只有超时的那一张");
    // 没有归档：撤不掉的那张单还在，状态停在「停止中」。
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopping
    );
    assert_eq!(harness.working().len(), 1);
    // 页面上看得到它在等什么。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.summary.status, StrategyStatus::Stopping);
    assert!(
        detail
            .note
            .as_deref()
            .is_some_and(|note| note.contains("没撤掉")),
        "unexpected note: {:?}",
        detail.note
    );

    // 常驻服务每一轮继续撤：这一轮撤得掉，才归档。
    let report = harness.step(2).await;
    assert_eq!(report.status, StrategyStatus::Stopped);
    assert_eq!(report.canceled.len(), 1);
    assert!(harness.working().is_empty());
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopped
    );
}

/// 撤销的回执丢了也不能一直卡着：撤完按 `cloid` 问一次终态，问出结局就归档。
///
/// 只重发撤销是问不出结局的——交易所会说「不认识这张单」，而它可能早就成交或者早就
/// 撤掉了。用户反复点停止，每次重发的都是同一句「撤掉它」，永远收不了尾。
#[tokio::test]
async fn b45_a_lost_cancel_receipt_still_archives_the_strategy() {
    let harness = harness().await;
    harness.step(0).await;
    let working: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();

    // 撤单请求全部超时：我们不知道它们撤掉了没有。
    harness
        .exchange
        .unknown_next_cancels(working.len(), "timed out");
    let canceled = harness.stop(1).await;
    assert!(canceled.is_empty(), "一次撤单回执都没收到");
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopping
    );

    // 交易所上这些单其实已经不在场了（比如撤单请求到了、回执丢在路上）。
    for client_order_id in &working {
        assert!(harness.exchange.cancel_externally(client_order_id));
    }

    // 再停一次：这一次按 `cloid` 问出了终态，订单结束、成交对完账，归档。
    let report = harness.step(2).await;
    assert_eq!(report.status, StrategyStatus::Stopped);
    assert!(report.canceled.is_empty(), "它们早就不在场了");
    assert!(harness.working().is_empty());
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopped
    );
}

/// 撤销之前成交的那部分要先记账，记完才归档；不等成交流水（第 9.3 节）。
///
/// 撤销只说明订单不再在场，撤销前可能已经有成交。按 `cloid` 问出撤销时的累计成交量、
/// 记进格子之后才归档：没记账就归档，那段成交再也没有人补记。流水只是审计记录，晚到
/// 不挡归档。
#[tokio::test]
async fn b45_fills_are_booked_before_the_strategy_is_archived() {
    let harness = harness().await;
    harness.step(0).await;
    let cell0_buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    // 成交记录比订单状态晚到。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&cell0_buy, Quantity::parse("0.03").unwrap(), at(10))
        .unwrap();

    let canceled = harness.stop(11).await;

    assert_eq!(canceled.len(), 20);
    assert!(harness.working().is_empty(), "订单已经不在场了");
    assert_eq!(
        harness.order(&cell0_buy).record.cumulative_filled,
        Quantity::parse("0.03").unwrap(),
        "撤销前的成交按订单状态记了账"
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.02").unwrap()
    );
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.03").unwrap()
    );
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopped,
        "记完账就归档，不等流水"
    );
}

/// 暂停也一样：撤不干净就停在「暂停中」，撤干净了才转「已暂停」。
#[tokio::test]
async fn pausing_waits_for_the_cancels_to_settle() {
    let harness = harness().await;
    harness.step(0).await;
    harness.exchange.unknown_next_cancel("timed out");

    let canceled = harness.pause(1).await;

    assert_eq!(canceled.len(), 19);
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Pausing
    );
    assert_eq!(harness.working().len(), 1);
    // 暂停中不下任何新单。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert!(detail.summary.working_orders == 1);

    let report = harness.step(2).await;
    assert_eq!(report.status, StrategyStatus::Paused);
    assert!(harness.working().is_empty());
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
}

/// B45：停止之后同一个标的可以重新建一个策略。
#[tokio::test]
async fn b45_stopping_frees_the_instrument_for_a_new_strategy() {
    let console = common::console();
    let draft_id = console
        .runtime
        .create_draft(common::long_grid_config())
        .unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    let first = console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console.clock.set(at(1));
    console.runtime.step(&first).await.unwrap();
    console.runtime.stop(&first).await.unwrap();

    let second_draft = console
        .runtime
        .create_draft(common::long_grid_config())
        .unwrap();
    let preview = console.runtime.preview_draft(&second_draft).await.unwrap();
    console
        .runtime
        .start_draft(&second_draft, preview.token.as_deref().unwrap())
        .await
        .unwrap();
}

/// 只撤单的步骤不能被展示用的查询卡住：行情查不到，挂单照样撤。
///
/// 行情、可用资金、仓位只进视图快照。用户点了暂停，在场挂单就该撤掉，不该因为行情源
/// 抖了一下就一张都不撤——那样挂单会一直留在交易所上，用户看到的是「暂停失败」。
#[tokio::test]
async fn a_pause_cancels_the_orders_even_when_the_market_price_query_fails() {
    let harness = harness().await;
    harness.step(0).await;
    let before = harness.runtime.detail(&harness.strategy_id).unwrap();

    harness.exchange.fail_next_mark_price("行情源不可用");
    let canceled = harness.pause(1).await;

    assert_eq!(canceled.len(), 20, "撤单不依赖行情");
    assert!(harness.working().is_empty());
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working()),
        "交易所上的挂单也要真的撤掉"
    );
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );

    // 没读到的读数沿用上一份，说明里写清楚为什么没更新。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.market_price, before.market_price);
    assert_eq!(detail.available_collateral, before.available_collateral);
    let note = detail.note.unwrap_or_default();
    assert!(note.contains("行情源不可用"), "unexpected note: {note}");
    assert!(note.contains("沿用上一份"), "unexpected note: {note}");
}

/// 只撤单的步骤不能被展示用的查询卡住：可用资金查不到，挂单照样撤、照样归档。
#[tokio::test]
async fn a_stop_archives_the_strategy_even_when_the_collateral_query_fails() {
    let harness = harness().await;
    harness.step(0).await;
    let before = harness.runtime.detail(&harness.strategy_id).unwrap();

    harness.account.fail_next_collateral("账户接口不可用");
    let canceled = harness.stop(1).await;

    assert_eq!(canceled.len(), 20, "撤单不依赖可用资金");
    assert!(harness.working().is_empty());
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working()),
        "交易所上的挂单也要真的撤掉"
    );
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopped
    );

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.available_collateral, before.available_collateral);
    let note = detail.note.unwrap_or_default();
    assert!(note.contains("账户接口不可用"), "unexpected note: {note}");
}

/// 遗留问题：对运行中的策略调用恢复不能撤单重挂——它从来没停过。
#[tokio::test]
async fn resuming_a_running_strategy_leaves_its_orders_alone() {
    let harness = harness().await;
    harness.step(0).await;
    let placed_before = harness.exchange.place_count();
    let working_before: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();

    let outcome = harness.resume(1).await;
    assert_eq!(outcome, ResumeOutcome::AlreadyRunning);
    assert_eq!(harness.exchange.place_count(), placed_before);
    let working_after: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();
    assert_eq!(working_after, working_before);
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Running
    );
}

/// 暂停之后恢复：挂单按状态重新挂上，不是把撤掉的单原样发回去。
#[tokio::test]
async fn resuming_a_paused_strategy_replaces_its_orders_with_new_ones() {
    let harness = harness().await;
    harness.step(0).await;
    let before: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();
    harness.pause(1).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );

    let outcome = harness.resume(2).await;
    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("expected a step, got {outcome:?}");
    };
    assert_eq!(report.placed.len(), 20);
    let after: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();
    assert_eq!(after.len(), 20);
    assert!(
        after
            .iter()
            .all(|client_order_id| !before.contains(client_order_id)),
        "resume must not resend the old orders"
    );
}

/// 重启后的恢复撤掉遗留挂单：那些单的结局数据库里无从得知。
#[tokio::test]
async fn recovering_a_running_strategy_cancels_the_leftover_orders() {
    let harness = harness().await;
    harness.step(0).await;
    let before: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();

    let report = harness.recover(6).await;

    assert_eq!(report.status, StrategyStatus::Running);
    assert_eq!(report.placed.len(), 20);
    let after: Vec<_> = harness
        .working()
        .into_iter()
        .map(|order| order.record.client_order_id)
        .collect();
    assert!(after.iter().all(|id| !before.contains(id)));
    // 遗留挂单在交易所上也真的撤掉了。
    assert!(harness.exchange.orders().iter().all(|order| {
        order
            .client_order_id
            .as_ref()
            .is_none_or(|client_order_id| !before.contains(client_order_id))
            || !order.status.is_working()
    }));
}

/// 遗留订单的撤销没确认就不进「运行中」：停在「恢复中」，一张新单都不下。
///
/// 撤销超时说明那些单可能还在交易所上。这时转成「运行中」并按状态重新挂单，同一个单位
/// 就可能有两张都会成交的单。收尾之后（撤销确认、按 `cloid` 核对出终态、成交对完账）
/// 才转成「运行中」。
#[tokio::test]
async fn a_resume_that_cannot_confirm_its_cancels_stays_in_restoring() {
    let harness = harness().await;
    harness.step(0).await;
    let placed_before = harness.exchange.place_count();

    // 重启前的挂单撤销全部超时：交易所那边它们还在。
    harness
        .exchange
        .unknown_next_cancels(harness.working().len(), "timed out");
    let report = harness.recover(6).await;

    assert_eq!(report.status, StrategyStatus::Restoring);
    assert!(report.placed.is_empty(), "撤销没确认就不该下新单");
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Restoring
    );
    assert_eq!(harness.working().len(), 20);
    assert_eq!(harness.exchange.place_count(), placed_before);

    // 下一步撤得掉了：撤单、按 `cloid` 核对终态、成交对账收尾之后才转「运行中」重新挂单。
    let report = harness.step(7).await;

    assert_eq!(report.status, StrategyStatus::Running);
    assert_eq!(report.placed.len(), 20);
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Running
    );
}

/// 用户点的「恢复」和启动恢复走同一条规则：先落到「恢复中」收尾遗留订单。
///
/// 暂停时没撤掉的订单还可能在场上，恢复不能直接开始下单。两条路走同一段代码，也就不会
/// 出现「网页上恢复会重复下单、重启恢复不会」这种差别。
#[tokio::test]
async fn a_user_resume_waits_for_the_leftovers_like_a_restart() {
    let harness = harness().await;
    harness.step(0).await;
    let placed_before = harness.exchange.place_count();

    // 暂停的撤单超时：状态停在「暂停中」，20 张单还在场。
    harness
        .exchange
        .unknown_next_cancels(harness.working().len(), "timed out");
    harness.pause(6).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Pausing
    );

    // 用户改主意点恢复：这一轮撤销又没确认，还是收尾不了。
    harness
        .exchange
        .unknown_next_cancels(harness.working().len(), "timed out");
    let outcome = harness.resume(7).await;

    assert!(
        matches!(outcome, ResumeOutcome::WaitingForSettle { .. }),
        "expected a waiting outcome, got {outcome:?}"
    );
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Restoring,
        "收尾没完成就留在「恢复中」，由后台继续推进"
    );
    assert_eq!(harness.working().len(), 20);
    assert_eq!(harness.exchange.place_count(), placed_before);
    // 页面上看得到它在等什么。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert!(
        detail
            .note
            .as_deref()
            .is_some_and(|note| note.contains("没撤掉")),
        "unexpected note: {:?}",
        detail.note
    );

    // 后台下一轮撤得掉：收干净了才恢复运行。
    let report = harness.step(8).await;

    assert_eq!(report.status, StrategyStatus::Running);
    assert_eq!(report.placed.len(), 20);
    assert_eq!(harness.working().len(), 20);
}

/// 只撤单的步骤也不能被订单查询卡住：挂单列表查不到，挂单照样撤。
///
/// 撤哪几张单靠库里存着的 `cloid` 就够，撤销请求发出去不需要先问到订单状态。查询失败
/// 只说明这一步不能宣告收场——终态和成交都还没核对出来，状态留在「暂停中」。
#[tokio::test]
async fn a_pause_cancels_the_orders_even_when_the_order_query_fails() {
    let harness = harness().await;
    harness.step(0).await;

    // 这一步里查挂单列表一直失败：同步那一次和撤完之后核对那一次都问不到。
    harness.exchange.fail_working_orders(2, "订单接口不可用");
    let canceled = harness.pause(1).await;

    assert_eq!(canceled.len(), 20, "撤单不依赖订单查询");
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working()),
        "交易所上的挂单也要真的撤掉"
    );
    // 终态没核对出来就不能宣告完成：停在「暂停中」，后台继续推。
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Pausing
    );
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let note = detail.note.unwrap_or_default();
    assert!(note.contains("订单接口不可用"), "unexpected note: {note}");

    // 下一轮查得到挂单列表了：核对出终态，才转「已暂停」。
    let report = harness.step(2).await;
    assert_eq!(report.status, StrategyStatus::Paused);
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
}

/// 成交流水查不到也一样：挂单照样撤，但这一轮不算收场。
///
/// 补拉成交失败说明这一段成交历史没核对完：撤销前可能已经有成交、成交记录也可能还没到。
/// 这时候归档，那段成交就再也没人补记了（第 9.2 节）。
#[tokio::test]
async fn a_stop_cancels_the_orders_even_when_the_fills_query_fails() {
    let harness = harness().await;
    harness.step(0).await;
    let before = harness.runtime.detail(&harness.strategy_id).unwrap();

    // 这一步补拉成交失败。撤单和「按 cloid 核对终态」不依赖流水，照常走完。
    harness.exchange.fail_fills_since(1, "成交流水接口不可用");
    let canceled = harness.stop(1).await;

    assert_eq!(canceled.len(), 20, "撤单不依赖成交流水");
    assert!(harness.working().is_empty());
    // 同步没走通，不宣告归档：状态留在「停止中」，后台下一轮再核对。
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopping
    );
    // 没读到的读数沿用上一份，说明里写清楚为什么没更新。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.market_price, before.market_price);
    let note = detail.note.unwrap_or_default();
    assert!(
        note.contains("成交流水接口不可用"),
        "unexpected note: {note}"
    );

    // 下一轮查得到流水了：核对完就归档。
    let report = harness.step(2).await;
    assert_eq!(report.status, StrategyStatus::Stopped);
}

/// 成交记账写库失败之后，内存里那份改动不能拿来撤单。
///
/// 补记成交先写流水、再写格子和订单的累计成交量，两步不是一个事务。写第二步失败时内存里
/// 已经改过了：继续拿它撤单，撤销会把订单的新累计成交量单独写进库、格子却停在原处——
/// 下一轮核对时累计量已经相等，永远不会补记，这一格的仓位就永久对不上了（第 9.2 节）。
#[tokio::test]
async fn a_failed_fill_write_leaves_the_cell_and_the_order_in_step() {
    let harness = harness().await;
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let filled = Quantity::parse("0.02").unwrap();
    let to_buy_back_before = harness.cell(0).to_buy_back;

    // 交易所上成交了一部分，本地的成交记账写库失败。
    harness.exchange.fill(&entry, filled, at(5)).unwrap();
    harness.runtime.store().fail_applied_fills(1);
    let canceled = harness.pause(6).await;

    // 撤单照做：撤哪几张单只看库里存着的 `cloid`。
    assert_eq!(canceled.len(), 20);
    assert!(harness.working().is_empty());
    // 同步没走通就不算收场：状态留在「暂停中」，后台下一轮再核对。
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Pausing
    );
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let note = detail.note.unwrap_or_default();
    assert!(
        note.contains("成交流水和订单状态"),
        "unexpected note: {note}"
    );
    // 格子和订单的累计成交量一起前进：一个记了、另一个没记，就是永久对不上。
    assert_eq!(harness.order(&entry).record.cumulative_filled, filled);
    assert_eq!(
        harness.cell(0).to_buy_back,
        to_buy_back_before.saturating_sub(filled)
    );

    // 下一轮把账对完，才转「已暂停」，两边的数还是对的。
    let report = harness.step(7).await;
    assert_eq!(report.status, StrategyStatus::Paused);
    assert_eq!(harness.order(&entry).record.cumulative_filled, filled);
    assert_eq!(
        harness.cell(0).to_buy_back,
        to_buy_back_before.saturating_sub(filled)
    );
}

/// 仓位核对最多比几轮（与运行时一致）：仓位接口在这几轮里一直报旧值。
const POSITION_CHECK_ROUNDS: usize = 3;

/// 撤单之后的核对写库失败，同样不能拿改过的内存接着用。
///
/// 撤单请求在路上时成交了一部分，流水还没到；撤完按 `cloid` 核对时把这笔成交记进内存，
/// 写库却失败了。这时如果接着取展示读数，而仓位接口恰好还是旧值，仓位核对会再核对一遍
/// 订单：内存里的累计量已经追平，订单那一笔被单独写进库、格子没跟上，之后流水到了也
/// 不会再补记——这一格就永久对不上了。
#[tokio::test]
async fn a_failed_fill_write_after_the_cancels_is_not_carried_into_the_readings() {
    let harness = harness().await;
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let filled = Quantity::parse("0.02").unwrap();
    let to_buy_back_before = harness.cell(0).to_buy_back;
    let position_before = harness.exchange.current_position();

    // 撤单请求在路上时成交了一部分，流水晚到；撤完之后的核对记账写库失败。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill_before_next_cancel(1, entry.clone(), filled, at(5));
    harness.runtime.store().fail_applied_fills(1);
    // 仓位接口还停在成交之前：仓位核对会认为数据还在变，再核对一遍订单。
    harness
        .exchange
        .report_stale_positions(POSITION_CHECK_ROUNDS, position_before);
    harness.pause(6).await;

    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Pausing
    );
    // 库里的格子和订单停在同一处：都还没记这笔成交。
    assert_eq!(
        harness.order(&entry).record.cumulative_filled,
        Quantity::ZERO
    );
    assert_eq!(harness.cell(0).to_buy_back, to_buy_back_before);

    // 流水到了：下一轮把这笔成交一起记上，才转「已暂停」。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(7).await;
    assert_eq!(report.status, StrategyStatus::Paused);
    assert_eq!(harness.order(&entry).record.cumulative_filled, filled);
    assert_eq!(
        harness.cell(0).to_buy_back,
        to_buy_back_before.saturating_sub(filled)
    );
}

/// 恢复也不能被订单查询卡住：成交和挂单一直查不到，遗留挂单照样撤。
///
/// 撤哪几张单只靠库里存着的 `cloid`，撤销请求发出去不需要先问到订单状态。查询失败只说明
/// 这一轮不能算收场：状态留在「恢复中」，后台下一轮接着撤、接着核对。
#[tokio::test]
async fn a_resume_cancels_the_leftovers_even_when_the_order_queries_keep_failing() {
    let harness = harness().await;
    harness.step(0).await;

    // 重启恢复，可交易所的订单接口一直不可用：成交流水和挂单列表都问不到。
    harness.exchange.fail_fills_since(100, "成交流水接口不可用");
    harness.exchange.fail_working_orders(100, "订单接口不可用");
    harness.clock.set(at(600));
    harness
        .runtime
        .hold_for_restore(&harness.strategy_id, "测试：进程重启")
        .await
        .unwrap();
    let error = harness
        .runtime
        .step(&harness.strategy_id)
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("成交流水接口不可用"),
        "unexpected error: {error}"
    );
    // 撤销真的发出去了：交易所上的挂单一张不剩。
    assert!(harness.working().is_empty());
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working()),
        "交易所上的挂单也要真的撤掉"
    );
    // 没收场：留在「恢复中」，一张新单都不下。
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Restoring
    );
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let note = detail.note.unwrap_or_default();
    assert!(
        note.contains("成交流水接口不可用"),
        "unexpected note: {note}"
    );
}

/// 沿用上一份读数时，读数时刻也沿用：把旧仓位标成刚取的，用户就不知道它有多旧。
///
/// 状态什么时候变的记在别处（`strategies.updated_at`），不必挤占读数时刻。
#[tokio::test]
async fn keeping_the_previous_readings_keeps_their_timestamp() {
    let harness = harness().await;
    harness.step(0).await;
    let before = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(before.taken_at, at(0));

    // 十分钟后停止，行情查不到：读数沿用上一份。
    harness.exchange.fail_next_mark_price("行情源不可用");
    harness.stop(600).await;

    let after = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(
        after.taken_at,
        at(0),
        "沿用的是那一刻的读数，就还是那一刻的时刻"
    );
    assert_eq!(after.market_price, before.market_price);
    assert_eq!(after.available_collateral, before.available_collateral);
    // 状态确实变了，变化时间记在策略记录上。
    assert_eq!(after.summary.status, StrategyStatus::Stopped);
    assert_eq!(
        harness
            .runtime
            .store()
            .load_strategy(&harness.strategy_id)
            .unwrap()
            .updated_at,
        at(600)
    );
}

/// 恢复中遇到一张结果从来没拿到过的订单：留在「恢复中」，一张新单都不下。
///
/// 撤销只会得到「查无此单」，问不出它是已成交、已撤销还是压根没落地（第 9.2 节）。
/// 它可能已经成交，这时候重新挂单就会在同一个单位上多出一张会成交的单。
#[tokio::test]
async fn a_restart_recovery_waits_for_an_order_whose_result_was_never_learned() {
    let harness = harness().await;
    // 下单超时：交易所根本没有这张单。
    harness.exchange.unknown_next_place("timed out");
    harness.step(0).await;
    let placed_before = harness.exchange.place_count();

    let report = harness.recover(6).await;

    assert_eq!(report.status, StrategyStatus::Restoring);
    assert!(report.placed.is_empty(), "结果没查清楚就不该下新单");
    assert_eq!(harness.exchange.place_count(), placed_before);
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Restoring
    );
    // 页面上写清楚为什么恢复不了：不是「正在重试」，是问不出结局。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let note = detail.note.unwrap_or_default();
    assert!(note.contains("查无此单"), "unexpected note: {note}");

    // 后台每一轮继续按 `cloid` 核对，可它问不出结局：一直停在「恢复中」。
    let report = harness.step(7).await;
    assert_eq!(report.status, StrategyStatus::Restoring);
    assert!(report.placed.is_empty());
    assert_eq!(harness.exchange.place_count(), placed_before);
}

/// 暂停不改变参数，也不动仓位。
#[tokio::test]
async fn pausing_keeps_the_position_and_the_config() {
    let harness = harness().await;
    harness.step(0).await;
    let cell0_buy = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness
        .exchange
        .fill(&cell0_buy, Quantity::parse("0.05").unwrap(), at(10))
        .unwrap();
    harness.step(11).await;
    let config = harness.config();
    harness
        .exchange
        .set_market_price(Price::parse("95500").unwrap(), at(11));

    harness.pause(12).await;

    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.05").unwrap()
    );
    assert_eq!(harness.config(), config);
    assert_eq!(harness.cell(0).to_buy_back, Quantity::ZERO);
}
