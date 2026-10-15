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
    let cell0_buy = harness.working_cloid(0, OrderPurpose::GridBuy);
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

/// 撤销之前成交的那部分要先补记，成交流水到齐之前不归档。
///
/// 撤销只说明订单不再在场，撤销前可能已经有成交、成交记录也可能还没到。没对完账就归档，
/// 那段成交再也没有人补记，仓位和记录就此对不上。
#[tokio::test]
async fn b45_fills_are_backfilled_before_the_strategy_is_archived() {
    let harness = harness().await;
    harness.step(0).await;
    let cell0_buy = harness.working_cloid(0, OrderPurpose::GridBuy);
    // 成交记录比订单状态晚到：这一笔要等下一次查成交时才看得见。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&cell0_buy, Quantity::parse("0.03").unwrap(), at(10))
        .unwrap();

    // 点停止：订单撤掉了，交易所认到的成交量也补记进了格子，但成交流水还没到，
    // 所以先不归档。
    let canceled = harness.stop(11).await;
    assert_eq!(canceled.len(), 20);
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopping
    );
    assert!(harness.working().is_empty(), "订单已经不在场了");
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.03").unwrap()
    );
    assert!(
        harness
            .runtime
            .fills(&harness.strategy_id)
            .unwrap()
            .is_empty(),
        "成交流水还没到，对账没完成"
    );

    // 成交流水到了：对完账才归档。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(12).await;

    assert_eq!(report.status, StrategyStatus::Stopped);
    assert_eq!(
        harness.runtime.fills(&harness.strategy_id).unwrap().len(),
        1,
        "撤销前的那笔成交留下了流水"
    );
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.03").unwrap()
    );
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopped
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
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !before.contains(&order.client_order_id) || !order.status.is_working())
    );
}

/// 暂停不改变参数，也不动仓位。
#[tokio::test]
async fn pausing_keeps_the_position_and_the_config() {
    let harness = harness().await;
    harness.step(0).await;
    let cell0_buy = harness.working_cloid(0, OrderPurpose::GridBuy);
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
