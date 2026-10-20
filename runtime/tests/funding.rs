//! 资金费（第 7.6 节）：当前资金费率、对本策略不利且年化超过阈值时提醒。
//!
//! 资金费率由常驻服务单独定时去读（`refresh_funding_rates`），交易的一步只看存下的读数。

mod common;

use common::{Harness, at, harness, short_harness};
use poise_runtime::settings::AccountSettings;
use poise_strategy::order::OrderPurpose;
use poise_strategy::units::{Notional, Price, Rate};

fn notional(value: &str) -> Notional {
    Notional::parse(value).unwrap()
}

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
    assert_eq!(note(&long), "资金费率不利：年化 87.60%（阈值 50.00%）。");

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

/// 累计资金费只算策略建立之后的；网格已实现利润按平仓腿成交量和计划价估算；比例是
/// 资金费 ÷ 利润，带符号（付出去是负的）。
#[tokio::test]
async fn the_funding_is_compared_with_the_realized_grid_profit() {
    let harness = harness().await;
    harness.step(0).await;
    let quantity = harness.runtime.detail(&harness.strategy_id).unwrap().cells[0].quantity;
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness.exchange.fill(&entry, quantity, at(1)).unwrap();
    harness.step(2).await;
    let exit = harness.working_cloid(0, OrderPurpose::GridExit);
    harness.exchange.fill(&exit, quantity, at(3)).unwrap();
    harness.step(4).await;

    // 建立之前的那笔不算。
    harness
        .account
        .add_funding_payment(at(-3600), notional("-9"));
    harness
        .account
        .add_funding_payment(at(3600), notional("-1.5"));
    harness
        .account
        .add_funding_payment(at(7200), notional("0.5"));
    harness.clock.set(at(7300));
    harness.runtime.refresh_funding_rates().await.unwrap();

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let profit = detail.cells[0].profit_maker;
    assert_eq!(detail.funding_total, notional("-1"));
    assert_eq!(detail.funding_synced_through, Some(at(7300)));
    assert_eq!(detail.grid_realized_profit, profit);
    let share = notional("-1").raw() * 100_000_000 / profit.raw();
    assert_eq!(detail.funding_share, Some(Rate::from_units(share as i64)));

    // 再同步一次：重叠的那段不会重复计入。
    harness.clock.set(at(7400));
    harness.runtime.refresh_funding_rates().await.unwrap();
    assert_eq!(
        harness
            .runtime
            .detail(&harness.strategy_id)
            .unwrap()
            .funding_total,
        notional("-1")
    );
}

/// 还没有网格利润时没有比例可算。
#[tokio::test]
async fn there_is_no_share_without_a_realized_profit() {
    let harness = harness().await;
    harness.step(0).await;
    harness
        .account
        .add_funding_payment(at(3600), notional("-1"));
    harness.clock.set(at(3700));
    harness.runtime.refresh_funding_rates().await.unwrap();

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.funding_total, notional("-1"));
    assert_eq!(detail.grid_realized_profit, Notional::ZERO);
    assert_eq!(detail.funding_share, None);
}

/// 归档的策略按实际收场时刻截止资金费：撤单、对账跨过了结算时刻，那一笔也算这个策略的；
/// 归档之后同步点越过「归档时刻 + 重查窗口」之前还会补同步，归档前结算、晚到的那笔不会丢。
/// 归档之后的结算不算。
#[tokio::test]
async fn an_archived_strategy_keeps_the_funding_up_to_when_it_was_archived() {
    let harness = harness().await;
    harness.step(0).await;
    harness.runtime.refresh_funding_rates().await.unwrap();

    // 第 3595 秒点停止；这一轮读行情花了 10 秒，第 3605 秒才收场。
    harness
        .account
        .add_funding_payment(at(3600), notional("-1.5"));
    harness
        .exchange
        .slow_mark_price(harness.clock.clone(), chrono::Duration::seconds(10));
    harness.stop(3595).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        poise_runtime::StrategyStatus::Stopped
    );

    // 归档之后的那笔不算；归档前那笔这时才同步到，照样算进去。
    harness
        .account
        .add_funding_payment(at(7200), notional("-4"));
    harness.clock.set(at(7300));
    harness.runtime.refresh_funding_rates().await.unwrap();

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.funding_total, notional("-1.5"));
    assert_eq!(detail.funding_synced_through, Some(at(7300)));
}

/// 账本持仓的均价、浮动盈亏、合计和保本价：持有一格时均价就是这一格的建仓价；价格跌 500
/// （不到下一格）就按 500 浮亏；合计把资金费算进去，保本价跟着往上挪。
#[tokio::test]
async fn the_detail_shows_the_holding_cost_and_the_total() {
    let harness = harness().await;
    harness.step(0).await;
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.average_price, None);
    assert_eq!(detail.total_pnl, Notional::ZERO);
    assert_eq!(detail.breakeven_price, None);

    let cell = detail.cells[0].clone();
    let entry = harness.working_cloid(0, OrderPurpose::GridEntry);
    harness.exchange.fill(&entry, cell.quantity, at(1)).unwrap();
    let lower = Price::from_units(cell.entry_price.units() - 50_000_000_000);
    harness.exchange.set_market_price(lower, at(2));
    harness.step(2).await;
    harness
        .account
        .add_funding_payment(at(3600), notional("-1"));
    harness.clock.set(at(3700));
    harness.runtime.refresh_funding_rates().await.unwrap();

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let loss = Notional::of(cell.quantity, Price::parse("500").unwrap());
    assert_eq!(detail.market_price, lower);
    assert_eq!(detail.average_price, Some(cell.entry_price));
    assert_eq!(detail.unrealized_pnl, Notional::ZERO - loss);
    assert_eq!(detail.reduction_realized_profit, None);
    assert_eq!(detail.total_pnl, Notional::ZERO - loss - notional("1"));
    // 付出去的 1 摊到持仓上：保本价 = 建仓价 + 1 ÷ 数量。
    let raised = Notional::of(cell.quantity, cell.entry_price) + notional("1");
    let units = i128::from(cell.quantity.units());
    assert_eq!(
        detail.breakeven_price,
        Some(Price::from_units(
            ((2 * raised.raw() + units) / (2 * units)) as i64
        ))
    );
}
