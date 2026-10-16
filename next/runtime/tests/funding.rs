//! 资金费（第 7.6 节）：当前资金费率、对本策略不利且年化超过阈值时提醒。
//!
//! 资金费率由常驻服务单独定时去读（`refresh_funding_rates`），交易的一步只看存下的读数。

mod common;

use common::{Harness, at, harness, short_harness};
use poise_runtime::settings::AccountSettings;
use poise_strategy::units::Rate;

fn rate(value: &str) -> Rate {
    Rate::parse(value).unwrap()
}

fn note(harness: &Harness) -> String {
    harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap_or_default()
}

/// 先读一次资金费率，再走一步。
async fn step_with_funding(harness: &Harness, hourly: &str, second: i64) {
    harness.account.set_funding_rate(rate(hourly));
    harness.clock.set(at(second));
    harness.runtime.refresh_funding_rates().await.unwrap();
    harness.step(second).await;
}

/// 详情显示当前资金费率：每小时的费率、换算出的年化（× 24 × 365），以及读到的时刻。
#[tokio::test]
async fn the_funding_rate_is_shown_hourly_and_annualized() {
    let harness = harness().await;

    step_with_funding(&harness, "0.0000125", 0).await;

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let funding = detail.funding_rate.unwrap();
    assert_eq!(funding.hourly, rate("0.0000125"));
    assert_eq!(funding.annual, rate("0.1095"));
    assert_eq!(detail.funding_rate_read_at, Some(at(0)));
}

/// 资金费率对本策略不利、年化超过提醒阈值（默认 50%）时提醒；有利的方向不提醒。
#[tokio::test]
async fn an_adverse_funding_rate_above_the_threshold_warns() {
    // 做多：正费率由多头付。0.0001 每小时，年化 87.6%。
    let long = harness().await;
    step_with_funding(&long, "0.0001", 0).await;
    assert!(note(&long).contains("资金费率"), "{}", note(&long));

    // 同样的费率对做空有利：不提醒。
    let short = short_harness().await;
    step_with_funding(&short, "0.0001", 0).await;
    assert!(!note(&short).contains("资金费率"), "{}", note(&short));

    // 负费率由空头付：做空提醒。
    let short = short_harness().await;
    step_with_funding(&short, "-0.0001", 0).await;
    assert!(note(&short).contains("资金费率"), "{}", note(&short));
}

/// 阈值在账户风控设置里可以改：调高之后同样的费率不再提醒。
#[tokio::test]
async fn the_funding_alert_threshold_is_an_account_setting() {
    let harness = harness().await;
    harness
        .runtime
        .update_account_settings(AccountSettings {
            funding_alert_annual_rate: rate("1"),
            ..AccountSettings::default()
        })
        .unwrap();

    step_with_funding(&harness, "0.0001", 0).await;

    assert!(!note(&harness).contains("资金费率"), "{}", note(&harness));
}

/// 交易的一步从不去读资金费率：读接口慢或者超时不能拖住撤单和止损。
#[tokio::test]
async fn a_step_never_reads_the_funding_rate() {
    let harness = harness().await;

    for second in [0, 3, 600, 3600] {
        harness.step(second).await;
    }

    assert_eq!(harness.account.funding_rate_reads(), 0);
    assert_eq!(
        harness
            .runtime
            .detail(&harness.strategy_id)
            .unwrap()
            .funding_rate,
        None
    );
}

/// 读失败时沿用上一次的读数；半小时没读到新的就当读不到，不再按旧费率判断提醒。
#[tokio::test]
async fn a_funding_rate_older_than_half_an_hour_is_unknown() {
    let harness = harness().await;
    step_with_funding(&harness, "0.0001", 0).await;
    assert!(note(&harness).contains("资金费率"));

    harness.account.fail_next_funding_rate("timeout");
    harness.clock.set(at(600));
    harness.runtime.refresh_funding_rates().await.unwrap();
    harness.step(600).await;
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.funding_rate_read_at, Some(at(0)));

    harness.step(1801).await;
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.funding_rate, None);
    assert!(!note(&harness).contains("资金费率"), "{}", note(&harness));
}

/// 详情按读详情这一刻的读数显示和提醒，不靠交易的一步：不走步、只推进时间，半小时后同样
/// 变成读不到，提醒也跟着消失。
#[tokio::test]
async fn the_detail_uses_the_current_reading_without_a_step() {
    let harness = harness().await;
    harness.step(0).await;
    harness.account.set_funding_rate(rate("0.0001"));
    harness.runtime.refresh_funding_rates().await.unwrap();

    assert!(note(&harness).contains("资金费率"), "{}", note(&harness));

    harness.clock.set(at(1801));
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.funding_rate, None);
    assert!(!note(&harness).contains("资金费率"), "{}", note(&harness));
}

/// 暂停的策略不走步，仓位还在付资金费：后台读到的新费率照样显示和提醒。
#[tokio::test]
async fn a_paused_strategy_still_shows_the_funding_rate() {
    let harness = harness().await;
    harness.step(0).await;
    harness.pause(1).await;

    harness.account.set_funding_rate(rate("0.0001"));
    harness.clock.set(at(2));
    harness.runtime.refresh_funding_rates().await.unwrap();

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.funding_rate.unwrap().hourly, rate("0.0001"));
    assert!(note(&harness).contains("资金费率"), "{}", note(&harness));
}
