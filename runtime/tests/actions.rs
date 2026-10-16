//! 每日操作次数（第 10 节，B30）：Unified 账户每天最多 5 万次用户操作。

mod common;

use common::{Harness, at, harness};
use poise_runtime::account::MarginReading;
use poise_runtime::{DAILY_ACTION_LIMIT, ResumeOutcome, StrategyStatus};
use poise_strategy::units::Notional;

/// 账户比率为 `percent`% 的保证金读数：余额 1000，没有逐仓，占用全在默认 DEX。
fn margin_at(percent: i64) -> MarginReading {
    let used = percent * 10 * 100_000_000;
    MarginReading {
        balance: Notional::parse("1000").unwrap(),
        available_after_maintenance: Notional::from_scaled(1000 * 100_000_000 - used),
        cross_maintenance: Notional::from_scaled(used),
        isolated_margin: Notional::ZERO,
        as_of: None,
    }
}

fn note(harness: &Harness) -> String {
    harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap_or_default()
}

/// 每次下单、撤单记一次，按 UTC 日分开计数。
#[tokio::test]
async fn b30_every_place_and_cancel_counts_once_per_utc_day() {
    let harness = harness().await;

    let first = harness.step(0).await;
    let placed = first.placed.len() as u64;
    assert!(placed > 0);
    assert_eq!(harness.runtime.actions_today().unwrap(), placed);

    let canceled = harness.pause(1).await.len() as u64;
    assert!(canceled > 0);
    assert_eq!(harness.runtime.actions_today().unwrap(), placed + canceled);

    // 第二天从零开始。
    harness.clock.set(at(86_400));
    assert_eq!(harness.runtime.actions_today().unwrap(), 0);
}

/// 当日操作次数超过上限的 80%：照常运行，详情里提醒。
#[tokio::test]
async fn b30_more_than_80_percent_of_the_daily_actions_warns() {
    let harness = harness().await;
    harness
        .runtime
        .store()
        .record_actions(at(0).date_naive(), DAILY_ACTION_LIMIT * 8 / 10)
        .unwrap();

    let report = harness.step(0).await;

    assert_eq!(report.status, StrategyStatus::Running);
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap();
    assert!(note.contains("操作次数"), "{note}");
    assert!(
        harness
            .runtime
            .detail(&harness.strategy_id)
            .unwrap()
            .actions_today
            > DAILY_ACTION_LIMIT * 8 / 10
    );
}

/// 被交易所以操作次数超限拒绝：暂停策略并提醒，不再自动重试。
#[tokio::test]
async fn b30_a_rate_limited_order_pauses_the_strategy() {
    let harness = harness().await;
    harness
        .exchange
        .rate_limit_next_place("Too many cumulative requests sent");

    let report = harness.step(0).await;

    assert!(report.is_paused(), "{:?}", report.status);
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap();
    assert!(note.contains("超限"), "{note}");
}

/// 撤单被以操作次数超限拒绝：同样暂停。撤不掉的单留在「暂停中」继续撤；暂停原因一直
/// 显示到用户点恢复，不会被撤单进度的说明盖掉。
#[tokio::test]
async fn b30_a_rate_limited_cancel_pauses_and_the_reason_stays_until_resume() {
    let harness = harness().await;
    harness.step(0).await;
    // 保证金率超过停止阈值：这一步要撤掉买单，撤单被超限拒绝。
    harness.account.set_margin(margin_at(55));
    harness.exchange.rate_limit_cancels(usize::MAX);

    let report = harness.step(1).await;

    assert_eq!(report.status, StrategyStatus::Pausing);
    assert!(note(&harness).contains("超限"), "{}", note(&harness));

    // 额度恢复：下一轮撤干净，转成已暂停，原因还在。
    harness.exchange.rate_limit_cancels(0);
    assert_eq!(harness.step(2).await.status, StrategyStatus::Paused);
    assert!(note(&harness).contains("超限"), "{}", note(&harness));

    harness.account.set_margin(margin_at(0));
    assert!(matches!(harness.resume(3).await, ResumeOutcome::Resumed(_)));
    assert!(!note(&harness).contains("超限"), "{}", note(&harness));
}

/// 只有第一张撤单被以操作次数超限拒绝、后面的都撤掉了：这个信号也不能丢，策略照样暂停，
/// 被拒的那张在暂停的撤单轮里撤掉。
#[tokio::test]
async fn b30_a_single_rate_limited_cancel_still_pauses() {
    let harness = harness().await;
    harness.step(0).await;
    harness.account.set_margin(margin_at(55));
    harness.exchange.rate_limit_cancels(1);

    let report = harness.step(1).await;

    assert!(report.is_paused(), "{:?}", report.status);
    assert!(note(&harness).contains("超限"), "{}", note(&harness));
    assert!(harness.working().is_empty(), "{:?}", harness.working());
}
