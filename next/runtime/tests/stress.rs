//! 满仓压力测试（第 7.3 节，B34）。
//!
//! 设计第 3.2 节的做多例子满仓：`M = 1`，按挂单价算的均价 86900；价格到范围下沿 75000 时
//! 浮亏 11900，维持保证金（模拟账户只有一档 40 倍）937.5。保证金率 = 937.5 ÷（余额 − 11900）。

mod common;

use common::{at, console, core_long_config, neutral_console, neutral_grid_config};
use poise_runtime::account::MarginReading;
use poise_strategy::side::Direction;
use poise_strategy::stress::Liquidation;
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};

/// 余额为 `balance`、没有任何占用的保证金读数。
fn flat_account(balance: &str) -> MarginReading {
    let balance = Notional::parse(balance).unwrap();
    MarginReading {
        balance,
        available_after_maintenance: balance,
        cross_maintenance: Notional::ZERO,
        isolated_margin: Notional::ZERO,
        as_of: None,
    }
}

/// B34：满仓保证金率达到停止阈值（50%）就拒绝这份配置，并给出提醒阈值以内的最大仓位。
#[tokio::test]
async fn b34_a_full_position_above_the_stop_threshold_is_refused() {
    let console = console();
    // 937.5 ÷ (13000 − 11900) ≈ 85%。
    console.account.set_margin(flat_account("13000"));
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(!preview.validation.valid);
    assert!(preview.token.is_none(), "拒绝的配置确认不了");
    let reason = preview.validation.reason.unwrap();
    assert!(reason.contains("停止阈值"), "{reason}");
    assert!(reason.contains("最大仓位"), "{reason}");
    assert_eq!(preview.stress.len(), 1);
    assert!(preview.stress[0].margin_ratio >= Rate::parse("0.5").unwrap());
}

/// B34：30% 到 50% 之间照常可以确认，但给出提醒和建议的最大仓位。
#[tokio::test]
async fn b34_between_the_thresholds_the_preview_warns() {
    let console = console();
    // 937.5 ÷ (14500 − 11900) ≈ 36%。
    console.account.set_margin(flat_account("14500"));
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(preview.validation.valid, "{:?}", preview.validation.reason);
    assert!(preview.token.is_some());
    assert_eq!(preview.warnings.len(), 1);
    assert!(
        preview.warnings[0].contains("提醒阈值"),
        "{:?}",
        preview.warnings
    );
    assert!(
        preview.warnings[0].contains("36."),
        "{:?}",
        preview.warnings
    );
}

/// 资金充足：没有提醒；预览列出满仓时的强平价（这里价格到零也不会强平，没有强平价）。
#[tokio::test]
async fn b34_an_ample_account_shows_the_stress_without_warnings() {
    let console = console();
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(preview.validation.valid);
    assert!(preview.warnings.is_empty());
    let long = preview.stress[0];
    assert_eq!(long.direction, Direction::Long);
    assert_eq!(long.adverse_price, Price::parse("75000").unwrap());
    assert_eq!(long.position, Quantity::parse("1").unwrap());
    assert_eq!(long.average_entry, Price::parse("86900").unwrap());
    assert_eq!(long.liquidation, Liquidation::Never);
}

/// 余额只比浮亏多一点：强平价在范围下沿之上不远处——预览照样列出它。
#[tokio::test]
async fn b34_the_liquidation_price_is_listed() {
    let console = console();
    console.account.set_margin(flat_account("20000"));
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    // 20000 + X − 86900 = 0.0125 X：X ≈ 67747。
    let Liquidation::At { price: liquidation } = preview.stress[0].liquidation else {
        panic!("expected a liquidation price");
    };
    assert!(
        liquidation > Price::parse("67000").unwrap()
            && liquidation < Price::parse("68000").unwrap(),
        "{liquidation}"
    );
}

/// 中性网格两侧分开算：多头侧在下沿满仓、空头侧在上沿满仓。
#[tokio::test]
async fn b34_a_neutral_grid_stresses_both_sides() {
    let console = neutral_console();
    console
        .exchange
        .set_market_price(Price::parse("90").unwrap(), at(0));
    let draft_id = console.runtime.create_draft(neutral_grid_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(preview.validation.valid, "{:?}", preview.validation.reason);
    let directions: Vec<Direction> = preview.stress.iter().map(|side| side.direction).collect();
    assert_eq!(directions, vec![Direction::Long, Direction::Short]);
}

/// 读不到保证金数据就做不了压力测试：校验不过，稍后重新预览。
#[tokio::test]
async fn b34_without_a_margin_reading_the_preview_is_not_confirmable() {
    let console = console();
    console.account.fail_margin(1);
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(!preview.validation.valid);
    assert!(preview.token.is_none());
    assert!(preview.validation.reason.unwrap().contains("满仓压力测试"));
}

/// 标的上已有仓位：它的维持保证金已经在账户占用里，接管之后又算在本策略的满仓里，
/// 只算一次。已有的 .37 按当前标记价 95500 计成本（不是计划价），满仓成本 87862。
#[tokio::test]
async fn b34_an_existing_position_is_not_counted_twice() {
    let console = console();
    // 已有 .37 多单，按标记价 95500 维持保证金 .37 × 95500 ÷ 80 ≈ 441.69，全在账户占用里。
    console
        .exchange
        .set_position(Position::parse("0.37").unwrap());
    let balance = Notional::parse("16000").unwrap();
    let used = Notional::parse("441.69").unwrap();
    console.account.set_margin(MarginReading {
        balance,
        available_after_maintenance: Notional::from_scaled(
            balance.scaled_floor() - used.scaled_floor(),
        ),
        cross_maintenance: used,
        isolated_margin: Notional::ZERO,
        as_of: None,
    });
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    // 只算本策略满仓的 937.5：937.5 ÷ (16000 − 12862) ≈ 29.9%，不是再加 441.69 之后的 43.9%。
    assert!(preview.validation.valid, "{:?}", preview.validation.reason);
    let ratio = preview.stress[0].margin_ratio;
    assert!(
        ratio > Rate::parse("0.29").unwrap() && ratio < Rate::parse("0.31").unwrap(),
        "{ratio}"
    );
    assert_eq!(
        preview.stress[0].average_entry,
        Price::parse("87862").unwrap()
    );
}

/// 标的设成逐仓：本系统只支持全仓，预览拒绝。
#[tokio::test]
async fn b34_an_isolated_instrument_is_refused() {
    let console = console();
    console
        .account
        .set_leverage(poise_runtime::account::Leverage {
            value: 10,
            cross: false,
        });
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(!preview.validation.valid);
    assert!(preview.validation.reason.unwrap().contains("逐仓"));
}

/// 保证金读数已经过期：做不了压力测试，预览不能确认。
#[tokio::test]
async fn b34_a_stale_margin_reading_is_refused() {
    let console = console();
    console.account.set_margin(MarginReading {
        as_of: Some(at(-60)),
        ..flat_account("100000")
    });
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(!preview.validation.valid);
    assert!(preview.token.is_none());
}

/// 余额不够扛到下沿：拒绝原因直接说强平价，不说一个封顶的比率。
#[tokio::test]
async fn b34_a_position_that_would_be_liquidated_says_where() {
    let console = console();
    console.account.set_margin(flat_account("5000"));
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    let reason = preview.validation.reason.unwrap();
    assert!(reason.contains("之前就会强平"), "{reason}");
    assert!(!reason.contains("10000.00%"), "{reason}");
}

/// 保证金读数的时刻在未来（本机时钟回拨或读数异常）：证明不了它新鲜，同样拒绝。
#[tokio::test]
async fn b34_a_margin_reading_from_the_future_is_refused() {
    let console = console();
    console.account.set_margin(MarginReading {
        as_of: Some(at(60)),
        ..flat_account("100000")
    });
    let draft_id = console.runtime.create_draft(core_long_config()).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(!preview.validation.valid);
}
