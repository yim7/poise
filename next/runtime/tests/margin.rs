//! 账户保证金率闸门（第 7.2、7.4 节，B35、B35b、B36）。
//!
//! 保证金率读不到：不发新单，已在场的挂单保留。超过停止阈值：同一账户所有策略只发、只
//! 保留只减仓单，其余挂单撤掉；回落后自动恢复。超过提醒阈值：提醒用户。

mod common;

use common::{Harness, at, core_harness, harness, neutral_harness};
use poise_runtime::RuntimeError;
use poise_runtime::account::MarginReading;
use poise_runtime::settings::AccountSettings;
use poise_strategy::order::OrderPurpose;
use poise_strategy::planning::WaitReason;
use poise_strategy::side::Direction;
use poise_strategy::units::{Notional, Price, Rate};

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

/// 挂出全部买单，第 0 格的买单整份成交，下一步挂出它的卖单（单侧网格里是只减仓单）。
async fn hold_cell_zero(harness: &Harness) {
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let quantity = harness.order(&entry).record.quantity;
    harness.exchange.fill(&entry, quantity, at(1)).unwrap();
    harness.step(2).await;
    let exit = harness.working_cloid(0, OrderPurpose::GridExit);
    assert!(harness.order(&exit).record.reduce_only);
}

/// B36：保证金率读不到时本策略不发任何新订单，已在场的挂单保留；读到之后照常。
#[tokio::test]
async fn b36_no_new_orders_while_the_margin_ratio_is_unreadable() {
    let harness = harness().await;
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let quantity = harness.order(&entry).record.quantity;
    harness.exchange.fill(&entry, quantity, at(1)).unwrap();

    harness.account.fail_margin(1);
    let report = harness.step(2).await;

    assert!(report.placed.is_empty());
    assert!(report.canceled.is_empty(), "已在场的挂单保留");
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::MarginRatioUnavailable)
    );
    assert_eq!(
        harness
            .runtime
            .detail(&harness.strategy_id)
            .unwrap()
            .margin_ratio,
        None
    );

    let report = harness.step(3).await;
    assert!(
        report
            .placed
            .iter()
            .any(|cloid| harness.order(cloid).record.purpose == OrderPurpose::GridExit),
        "读到之后照常挂出卖单"
    );
}

/// B35：保证金率超过停止阈值，只保留只减仓单：主仓买单和格子买单撤掉，格子卖单（只减仓）
/// 留着；回落之后自动恢复挂单。
#[tokio::test]
async fn b35_above_the_stop_threshold_only_reduce_only_orders_stay() {
    let harness = core_harness().await;
    harness.step(0).await;
    // 95 批主仓买入，五格启用，挂出格子买单；95 格的买单成交，挂出它的卖单。
    let core = harness.working_core_cloid(0);
    let core_quantity = harness.order(&core).record.quantity;
    harness.exchange.fill(&core, core_quantity, at(1)).unwrap();
    harness.step(2).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let quantity = harness.order(&entry).record.quantity;
    harness.exchange.fill(&entry, quantity, at(3)).unwrap();
    harness.step(4).await;
    let exit = harness.working_cloid(0, OrderPurpose::GridExit);
    assert!(harness.order(&exit).record.reduce_only);
    let place_count = harness.exchange.place_count();

    harness.account.set_margin(margin_at(55));
    let report = harness.step(5).await;

    assert!(report.placed.is_empty(), "非只减仓单不发");
    let working = harness.working();
    assert!(
        working.iter().all(|order| order.record.reduce_only),
        "只保留只减仓单：{working:?}"
    );
    assert!(
        working
            .iter()
            .any(|order| order.record.client_order_id == exit)
    );
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::MarginRatioAboveStopThreshold)
    );
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.margin_ratio, Some(Rate::parse("0.55").unwrap()));
    let note = detail.note.unwrap_or_default();
    assert!(note.contains("停止阈值"), "unexpected note: {note}");
    assert_eq!(harness.exchange.place_count(), place_count);

    // 回落到停止阈值以下：自动恢复，撤掉的买单重新挂出。
    harness.account.set_margin(margin_at(20));
    let report = harness.step(6).await;
    assert!(!report.placed.is_empty());
    assert!(
        harness
            .working()
            .iter()
            .any(|order| !order.record.reduce_only),
        "买单回来了"
    );
}

/// 读到超过停止阈值之后，后面的读取失败也不能让增仓单留在场上：撤单放在这一步的其他
/// 读取之前，只靠库里的 `cloid`。
#[tokio::test]
async fn b35_increasing_orders_are_withdrawn_even_when_a_later_read_fails() {
    let harness = harness().await;
    harness.step(0).await;
    assert!(
        harness
            .working()
            .iter()
            .all(|order| !order.record.reduce_only)
    );

    harness.account.set_margin(margin_at(60));
    harness.exchange.fail_next_mark_price("行情接口不可用");
    harness.clock.set(at(1));
    assert!(harness.runtime.step(&harness.strategy_id).await.is_err());

    assert!(harness.working().is_empty(), "买单已经撤掉");
}

/// B36：交易所给的保证金读数已经过期，和读不到一样：不发新单，挂单保留。
#[tokio::test]
async fn b36_a_stale_margin_reading_closes_the_gate() {
    let harness = harness().await;
    harness.step(0).await;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    let quantity = harness.order(&entry).record.quantity;
    harness.exchange.fill(&entry, quantity, at(1)).unwrap();

    harness.account.set_margin(MarginReading {
        as_of: Some(at(-60)),
        ..margin_at(10)
    });
    let report = harness.step(2).await;

    assert!(report.placed.is_empty());
    assert!(report.canceled.is_empty());
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::MarginRatioUnavailable)
    );

    harness.account.set_margin(MarginReading {
        as_of: Some(at(3)),
        ..margin_at(10)
    });
    let report = harness.step(3).await;
    assert!(!report.placed.is_empty(), "读数新鲜了，照常挂出卖单");
}

/// 保证金读数读到时还有效、发单途中过期：按读数本身的时刻计算，过期之后不再发新单。
#[tokio::test]
async fn new_orders_stop_once_the_margin_reading_gets_stale() {
    let harness = harness().await;
    // 读到时已经旧了 9 秒；每发一张单花 2 秒，发完第一张就旧了 11 秒。
    harness.account.set_margin(MarginReading {
        as_of: Some(at(-9)),
        ..margin_at(10)
    });
    harness
        .exchange
        .slow_places(harness.clock.clone(), chrono::Duration::seconds(2));

    let report = harness.step(0).await;

    assert_eq!(report.placed.len(), 1);
    assert!(report.plan.waiting.contains(&WaitReason::StaleMarketData));
}

/// B35b：中性网格的格子单可能让净仓位穿越零，不是只减仓单；超过停止阈值时两侧的单都撤，
/// 不会出现卖出之后变成净空的情况。
#[tokio::test]
async fn b35b_a_neutral_grid_withdraws_both_sides_above_the_stop_threshold() {
    let harness = neutral_harness().await;
    harness
        .exchange
        .set_market_price(Price::parse("90").unwrap(), at(0));
    harness.step(0).await;
    // 多头 89 格买入、空头 91 格开空：两格各挂出平仓单。
    let long_entry = harness.working_cloid_of(Direction::Long, 0, OrderPurpose::GridEntry);
    let quantity = harness.order(&long_entry).record.quantity;
    harness.exchange.fill(&long_entry, quantity, at(1)).unwrap();
    let short_entry = harness.working_cloid_of(Direction::Short, 0, OrderPurpose::GridEntry);
    let quantity = harness.order(&short_entry).record.quantity;
    harness
        .exchange
        .fill(&short_entry, quantity, at(1))
        .unwrap();
    harness.step(2).await;
    assert!(
        harness
            .working()
            .iter()
            .any(|order| order.record.purpose == OrderPurpose::GridExit)
    );
    let position = harness.exchange.current_position();

    harness.account.set_margin(margin_at(60));
    harness.step(3).await;

    assert!(harness.working().is_empty(), "两侧的格子单全部撤掉");
    assert_eq!(harness.exchange.current_position(), position, "不发任何单");
}

/// 超过提醒阈值、没到停止阈值：照常下单，页面上提醒。
#[tokio::test]
async fn above_the_alert_threshold_the_strategy_runs_and_warns() {
    let harness = harness().await;
    harness.step(0).await;

    harness.account.set_margin(margin_at(35));
    let report = harness.step(1).await;

    assert!(report.plan.waiting.iter().all(|reason| {
        !matches!(
            reason,
            WaitReason::MarginRatioAboveStopThreshold | WaitReason::MarginRatioUnavailable
        )
    }));
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap_or_default();
    assert!(note.contains("提醒阈值"), "unexpected note: {note}");
    assert!(note.contains("35.00%"), "unexpected note: {note}");
}

/// 阈值是账户风控设置：改了马上生效；不合理的设置拒绝保存。
#[tokio::test]
async fn the_thresholds_come_from_the_account_settings() {
    let harness = harness().await;
    hold_cell_zero(&harness).await;

    let error = harness
        .runtime
        .update_account_settings(AccountSettings {
            margin_alert_ratio: Rate::parse("0.6").unwrap(),
            margin_stop_ratio: Rate::parse("0.5").unwrap(),
            ..AccountSettings::default()
        })
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Settings(_)), "{error:?}");
    assert_eq!(
        harness.runtime.account_settings().unwrap(),
        AccountSettings::default()
    );

    harness
        .runtime
        .update_account_settings(AccountSettings {
            margin_alert_ratio: Rate::parse("0.1").unwrap(),
            margin_stop_ratio: Rate::parse("0.2").unwrap(),
            ..AccountSettings::default()
        })
        .unwrap();
    harness.account.set_margin(margin_at(25));
    let report = harness.step(3).await;

    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::MarginRatioAboveStopThreshold)
    );
    assert!(
        harness
            .working()
            .iter()
            .all(|order| order.record.reduce_only)
    );
}
