//! 交易所侧的兜底止损单（`runtime::backstop`）与对外提醒。

mod common;

use std::sync::Arc;

use common::{Harness, at, core_long_config, harness_with};
use poise_runtime::{RecordingNotifier, Store, StrategyStatus};
use poise_strategy::config::{Invalidation, StrategyConfig};
use poise_strategy::units::{Position, Price, Quantity, Rate};

fn price(value: &str) -> Price {
    Price::parse(value).unwrap()
}

fn quantity(value: &str) -> Quantity {
    Quantity::parse(value).unwrap()
}

/// 做多主仓网格 `[75000, 95000]`，失效价 70000、剩余数量 .3：兜底单的触发价是 69300。
fn config() -> StrategyConfig {
    let mut config = core_long_config();
    config.long.as_mut().unwrap().invalidation = Some(Invalidation {
        price: price("70000"),
        remaining: quantity("0.3"),
        max_slippage: Rate::parse("0.01").unwrap(),
    });
    config
}

/// 起好策略、换上记录提醒的出口；第 0 秒挂好主仓买单，第 1 秒价格跌到 84500：
/// 95000、90000、85000 三批主仓成交。
async fn holding() -> (Harness, Arc<RecordingNotifier>) {
    let mut harness = harness_with(config(), Store::open_in_memory().unwrap()).await;
    let notices = RecordingNotifier::new();
    harness.runtime = harness.runtime.with_notifier(notices.clone());
    harness.step(0).await;
    assert!(harness.exchange.open_stops().is_empty(), "还没有仓位");
    harness.exchange.set_market_price(price("84500"), at(1));
    (harness, notices)
}

fn excess(harness: &Harness) -> Quantity {
    let held = Quantity::from_units(harness.exchange.current_position().units());
    held.saturating_sub(quantity("0.3"))
}

/// 有了超出 R 的仓位，下一步就在失效价外 1% 挂一张兜底单，数量是超出 R 的部分；详情页
/// 列出它。仓位变了就撤了按新数量重挂。兜底单在每一步开头对齐，所以这一步里新成交的
/// 格子要到下一步才算进去。
#[tokio::test]
async fn a_backstop_follows_the_position_above_the_remaining_quantity() {
    let (harness, _) = holding().await;

    harness.step(1).await;
    harness.step(2).await;

    let first = excess(&harness);
    assert!(first > Quantity::ZERO);
    assert_eq!(harness.exchange.open_stops(), vec![(price("69300"), first)]);
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.backstops.len(), 1);
    assert_eq!(detail.backstops[0].trigger_price, price("69300"));
    assert_eq!(detail.backstops[0].quantity, first);
    assert!(detail.backstops[0].confirmed);

    // 再跌一批：仓位变大，兜底单跟着换成新数量，交易所上始终只有一张。
    harness.exchange.set_market_price(price("79500"), at(3));
    harness.step(3).await;
    harness.step(4).await;

    let second = excess(&harness);
    assert!(second > first);
    assert_eq!(
        harness.exchange.open_stops(),
        vec![(price("69300"), second)]
    );
}

/// poise 不在的时候价格跌穿触发价：交易所执行兜底单。之后的一步发现它成交过，转失效，
/// 收尾完成后进入已失效，并推一条提醒。
#[tokio::test]
async fn a_backstop_triggered_while_poise_was_away_invalidates_the_strategy() {
    let (harness, notices) = holding().await;
    harness.step(1).await;
    assert_eq!(harness.exchange.open_stops().len(), 1);

    // 没有任何一步在跑，价格直接跌到 69000。
    harness.exchange.set_market_price(price("69000"), at(10));
    assert!(harness.exchange.open_stops().is_empty(), "兜底单已触发");

    let mut status = harness.step(11).await.status;
    for second in 12..20 {
        if status == StrategyStatus::Invalidated {
            break;
        }
        status = harness.step(second).await.status;
    }

    assert_eq!(status, StrategyStatus::Invalidated);
    assert!(harness.exchange.current_position() <= Position::parse("0.3").unwrap());
    assert!(
        notices.titles().contains(&"BTC 兜底单已触发".to_string()),
        "{:?}",
        notices.titles()
    );
    assert!(
        notices.titles().contains(&"BTC 已失效".to_string()),
        "{:?}",
        notices.titles()
    );
}

/// 价格已经越过失效价时不挂兜底单：挂上去会立即按市价成交，抢在 poise 按滑点上限减仓之前。
#[tokio::test]
async fn no_backstop_is_placed_once_the_price_has_crossed() {
    let mut harness = harness_with(config(), Store::open_in_memory().unwrap()).await;
    let notices = RecordingNotifier::new();
    harness.runtime = harness.runtime.with_notifier(notices.clone());
    harness.step(0).await;
    harness.exchange.set_market_price(price("69000"), at(1));

    let report = harness.step(1).await;

    assert_eq!(report.status, StrategyStatus::Invalidating);
    assert!(
        harness.exchange.stops().is_empty(),
        "{:?}",
        harness.exchange.stops()
    );
    assert!(
        notices.titles().contains(&"BTC 越过失效价".to_string()),
        "{:?}",
        notices.titles()
    );
}

/// 暂停只是先别做网格：兜底单留着。停止就是用户自己接管：撤掉兜底单之后才归档。
#[tokio::test]
async fn pausing_keeps_the_backstop_and_stopping_removes_it() {
    let (harness, _) = holding().await;
    harness.step(1).await;
    assert_eq!(harness.exchange.open_stops().len(), 1);

    harness.pause(2).await;
    harness.step(3).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
    assert_eq!(harness.exchange.open_stops().len(), 1, "暂停时兜底单留着");

    harness.resume(4).await;
    harness.stop(5).await;
    // 网页和命令行的停止不经过常驻驱动：停止这一下就要把兜底单撤掉。
    assert!(
        harness.exchange.open_stops().is_empty(),
        "停止当场撤掉兜底单"
    );
    let mut status = harness.runtime.status(&harness.strategy_id).unwrap();
    for second in 6..12 {
        if status == StrategyStatus::Stopped {
            break;
        }
        status = harness.step(second).await.status;
    }

    assert_eq!(status, StrategyStatus::Stopped);
    assert!(harness.exchange.open_stops().is_empty(), "归档前撤掉兜底单");
    assert!(
        harness
            .runtime
            .store()
            .load_backstops(&harness.strategy_id)
            .unwrap()
            .is_empty()
    );
}

/// 交易所拒绝兜底单：详情页说明原因，推一条提醒；同一个原因不重复推。
#[tokio::test]
async fn a_refused_backstop_is_shown_and_notified_once() {
    let (harness, notices) = holding().await;
    harness
        .exchange
        .reject_next_stop("Order has invalid price.");

    harness.step(1).await;

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let note = detail.note.unwrap_or_default();
    assert!(note.contains("兜底止损单挂不上"), "{note}");
    assert!(note.contains("Order has invalid price."), "{note}");
    assert_eq!(
        notices
            .titles()
            .iter()
            .filter(|title| *title == "BTC 兜底单挂不上")
            .count(),
        1
    );

    // 下一步交易所接受了：挂上，提醒消失。
    harness.step(2).await;
    assert_eq!(harness.exchange.open_stops().len(), 1);
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert!(!detail.note.unwrap_or_default().contains("兜底止损单挂不上"));
}

/// 仓位对不上而自动暂停时推一条提醒，说明原因。
#[tokio::test]
async fn an_automatic_pause_is_notified() {
    let (harness, notices) = holding().await;
    harness.step(1).await;

    harness.exchange.manual_trade(quantity("0.05"));
    // 差额要持续满宽限期（30 秒）才暂停：这一步记下差额，30 秒后那一步才暂停。
    harness.step(2).await;
    let report = harness.step(32).await;

    assert!(report.is_paused());
    let pause = notices
        .notices()
        .into_iter()
        .find(|notice| notice.title == "BTC 已暂停")
        .expect("a pause notice");
    assert!(
        pause.body.contains("仓位和应有持仓对不上"),
        "{}",
        pause.body
    );
}

/// 越过失效价的那一步，发减仓单之前先撤兜底单：否则同一波急跌里两张都成交，R > 0 时卖穿
/// 剩余数量。
#[tokio::test]
async fn crossing_the_invalidation_price_removes_the_backstop_before_reducing() {
    let (harness, _) = holding().await;
    harness.step(1).await;
    assert_eq!(harness.exchange.open_stops().len(), 1);

    // 跌过失效价 70000，还没到触发价 69300。
    harness.exchange.set_market_price(price("69500"), at(2));
    let report = harness.step(2).await;

    assert_eq!(report.status, StrategyStatus::Invalidating);
    assert!(!report.placed.is_empty(), "这一步就发了减仓单");
    assert!(
        harness.exchange.open_stops().is_empty(),
        "兜底单在同一步撤掉了"
    );

    // 价格接着跌穿触发价：兜底单已经不在，只剩失效减仓，仓位不会低于 R。
    harness.exchange.set_market_price(price("69000"), at(3));
    let mut status = report.status;
    for second in 3..12 {
        if status == StrategyStatus::Invalidated {
            break;
        }
        status = harness.step(second).await.status;
    }
    assert_eq!(status, StrategyStatus::Invalidated);
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.3").unwrap()
    );
}
