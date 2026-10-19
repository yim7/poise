//! 网页流程：新建草稿、预览、确认启动（第 2.4 节，B42、B43、B46）。
//!
//! 全程用模拟交易所和模拟账户，不连任何外部服务。

mod common;

use chrono::Duration;
use common::{at, console, long_grid_config, throttled};
use poise_runtime::{RuntimeError, StrategyStatus};
use poise_strategy::units::{Notional, Position, Price};

/// 令牌里的指纹部分：预览时刻不进指纹。
fn fingerprint(token: &str) -> &str {
    token
        .split_once('.')
        .unwrap_or_else(|| panic!("`{token}` is not a confirm token"))
        .1
}

/// B42：确认启动之前不下任何单，确认之后才按预览的样子开始挂单。
#[tokio::test]
async fn b42_a_draft_places_nothing_until_it_is_confirmed() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert!(preview.validation.valid, "{:?}", preview.validation.reason);
    assert_eq!(preview.config_version, 1);
    assert_eq!(preview.cells.len(), 20);
    assert!(preview.immediate_fills.is_empty());
    assert!(preview.token.is_some());

    // 预览只读：没有策略，没有订单。
    assert!(console.store.load_all_strategy_ids().unwrap().is_empty());
    assert_eq!(console.exchange.place_count(), 0);

    let strategy_id = console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    assert_eq!(strategy_id, draft_id);
    assert_eq!(
        console.runtime.status(&strategy_id).unwrap(),
        StrategyStatus::Running
    );
    // 草稿变成了策略。
    assert!(console.store.load_draft_ids().unwrap().is_empty());
    assert_eq!(console.runtime.drafts().unwrap().len(), 0);
    assert_eq!(console.exchange.place_count(), 0);

    // 确认之后走一步，挂出的单和预览说的格子对得上。
    console.runtime.step(&strategy_id).await.unwrap();
    let working: Vec<Price> = console
        .runtime
        .orders(&strategy_id)
        .unwrap()
        .into_iter()
        .filter(|order| order.record.is_working())
        .map(|order| order.record.limit_price)
        .collect();
    let mut expected: Vec<Price> = preview.cells.iter().map(|cell| cell.entry_price).collect();
    expected.sort();
    let mut working = working;
    working.sort();
    assert_eq!(working, expected);
}

/// B42：每次保存记一个参数版本，确认启动后策略接着这份版本历史。
#[tokio::test]
async fn b42_a_draft_keeps_every_saved_version() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();

    let mut second = long_grid_config();
    common::long_side_mut(&mut second).grid_spacing = Price::parse("500").unwrap();
    assert_eq!(
        console
            .runtime
            .save_draft(&draft_id, second.clone())
            .unwrap(),
        2
    );
    let third = throttled(second, Duration::minutes(60));
    assert_eq!(
        console
            .runtime
            .save_draft(&draft_id, third.clone())
            .unwrap(),
        3
    );
    assert_eq!(console.runtime.draft(&draft_id).unwrap().config, third);

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert_eq!(preview.config_version, 3);
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();

    let versions = console.runtime.config_versions(&draft_id).unwrap();
    assert_eq!(versions.len(), 3);
    assert_eq!(versions.last().unwrap().config, third);
}

/// B43：预览之后价格变了，变得会改变哪些订单立即成交，旧预览就不能再确认。
#[tokio::test]
async fn b43_a_price_move_that_changes_the_immediate_fills_requires_a_new_preview() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert!(preview.immediate_fills.is_empty());

    // 价格掉到最高买价以下：95000 那一格现在会立即成交。
    console
        .exchange
        .set_market_price(Price::parse("94999").unwrap(), at(10));
    let error = console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::PreviewStale(_)), "{error:?}");

    // 重新预览之后才能启动，令牌也换了。
    let fresh = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert_eq!(fresh.immediate_fills.len(), 1);
    assert_ne!(fresh.token, preview.token);
    console
        .runtime
        .start_draft(&draft_id, fresh.token.as_deref().unwrap())
        .await
        .unwrap();
}

/// B43：价格动了但没有改变任何会立即成交的订单，预览说明的还是同一件事，不用重来。
#[tokio::test]
async fn b43_a_price_move_that_changes_nothing_keeps_the_preview_valid() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    console
        .exchange
        .set_market_price(Price::parse("96000").unwrap(), at(10));
    let fresh = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert_eq!(
        fingerprint(fresh.token.as_deref().unwrap()),
        fingerprint(preview.token.as_deref().unwrap()),
        "标记价不进指纹"
    );
    assert_ne!(fresh.market_price, preview.market_price);
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
}

/// B43：预览超过 60 秒就失效，哪怕仓位、可用抵押和会立即成交的订单都没变。
///
/// 预览说的是「现在会发生什么」，超过一分钟的读数不该再拿来当确认的依据。
#[tokio::test]
async fn b43_a_preview_older_than_a_minute_requires_a_new_preview() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    let token = preview.token.as_deref().unwrap().to_string();

    // 59 秒：说明的还是同一件事。
    console.clock.set(at(59));
    let fresh = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert_eq!(
        fingerprint(fresh.token.as_deref().unwrap()),
        fingerprint(&token)
    );

    // 61 秒：过期，重新预览才行。
    console.clock.set(at(61));
    let error = console
        .runtime
        .start_draft(&draft_id, &token)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::PreviewStale(_)), "{error:?}");
    assert!(error.to_string().contains("超过 60 秒"), "{error}");
    assert!(console.store.load_all_strategy_ids().unwrap().is_empty());

    let fresh = console.runtime.preview_draft(&draft_id).await.unwrap();
    console
        .runtime
        .start_draft(&draft_id, fresh.token.as_deref().unwrap())
        .await
        .unwrap();
}

/// B43：令牌里的时刻在未来（时钟被回拨，或者时刻是编的）同样算过期。
///
/// 只判「大于 60 秒」的话，负的时长永远不大于它，60 秒这条上界就被绕过去了。
#[tokio::test]
async fn b43_a_token_from_the_future_is_refused() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    let token = preview.token.as_deref().unwrap();
    // 指纹原样不动，只有时刻被推到未来。
    let future = format!("{}.{}", at(60).timestamp_millis(), fingerprint(token));

    let error = console
        .runtime
        .start_draft(&draft_id, &future)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::PreviewStale(_)), "{error:?}");
    assert!(console.store.load_all_strategy_ids().unwrap().is_empty());
}

/// B43：预览之后账户变了（可用抵押少了），旧预览也不能再确认。
#[tokio::test]
async fn b43_an_account_change_requires_a_new_preview() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    console
        .account
        .set_collateral(Notional::parse("500").unwrap());
    let error = console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::PreviewStale(_)), "{error:?}");

    let fresh = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert_eq!(fresh.available_collateral, Notional::parse("500").unwrap());
    console
        .runtime
        .start_draft(&draft_id, fresh.token.as_deref().unwrap())
        .await
        .unwrap();
}

/// B43：可用抵押小幅波动不让预览过期。标的上有仓位时，统一账户的可用抵押随浮动盈亏
/// 每时每刻都在变；只有明显变少（低于预览时的 99%）才要求重新预览。多了没关系。
#[tokio::test]
async fn b43_a_small_collateral_move_keeps_the_preview_valid() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    let previewed = preview.available_collateral;

    // 少了 0.5%：还在容差之内。
    console
        .account
        .set_collateral(Notional::from_scaled(previewed.scaled_floor() / 1000 * 995));
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
}

/// B43：可用抵押多了，预览说明的那些单照样下得出去。
#[tokio::test]
async fn b43_more_collateral_keeps_the_preview_valid() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    console.account.set_collateral(Notional::from_scaled(
        preview.available_collateral.scaled_floor() * 2,
    ));
    console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap();
}

/// B43：预览之后仓位变了，旧预览也不能再确认。
#[tokio::test]
async fn b43_a_position_change_requires_a_new_preview() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert_eq!(preview.position, Position::ZERO);

    // 账户上多了一个仓位：手动开的，或者别的策略留下的。
    console
        .exchange
        .set_position(Position::parse("0.2").unwrap());
    let error = console
        .runtime
        .start_draft(&draft_id, preview.token.as_deref().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::PreviewStale(_)), "{error:?}");
    assert!(error.to_string().contains("仓位"), "{error}");

    let fresh = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert_eq!(fresh.position, Position::parse("0.2").unwrap());
    console
        .runtime
        .start_draft(&draft_id, fresh.token.as_deref().unwrap())
        .await
        .unwrap();
}

/// B43：令牌是确认的唯一凭据，随便编一个启动不了。
#[tokio::test]
async fn b43_a_forged_token_is_refused() {
    let console = console();
    let draft_id = console.runtime.create_draft(long_grid_config()).unwrap();
    console.runtime.preview_draft(&draft_id).await.unwrap();
    let error = console
        .runtime
        .start_draft(&draft_id, "not-a-token")
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::PreviewStale(_)), "{error:?}");
    assert!(console.store.load_all_strategy_ids().unwrap().is_empty());
}

/// B46：一个标的同时只能有一个活动（运行中或暂停）的策略。
#[tokio::test]
async fn b46_a_second_strategy_on_the_same_instrument_is_refused() {
    let console = console();
    let first = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&first).await.unwrap();
    console
        .runtime
        .start_draft(&first, preview.token.as_deref().unwrap())
        .await
        .unwrap();

    // 同一个标的的第二个草稿：预览照常，启动被拒。
    let second = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&second).await.unwrap();
    let error = console
        .runtime
        .start_draft(&second, preview.token.as_deref().unwrap())
        .await
        .unwrap_err();
    match error {
        RuntimeError::InstrumentBusy { strategy_id, .. } => assert_eq!(strategy_id, first),
        other => panic!("expected InstrumentBusy, got {other:?}"),
    }
    // 被拒之后草稿还在，可以改完再来。
    assert_eq!(console.runtime.drafts().unwrap().len(), 1);

    // 暂停的第一个策略还在场，仍然挡住第二个。
    console.clock.set(at(10));
    console.runtime.pause(&first).await.unwrap();
    let error = console
        .runtime
        .start_draft(&second, preview.token.as_deref().unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::InstrumentBusy { .. }),
        "{error:?}"
    );
}

/// 校验不过的草稿：预览给出原因，没有表格也没有令牌，启动不了。
#[tokio::test]
async fn a_draft_that_does_not_validate_previews_with_a_reason_and_no_token() {
    let console = console();
    let mut config = long_grid_config();
    // 价差 1 美元：扣掉手续费之后每格是亏的。
    common::long_side_mut(&mut config).grid_spread = Price::parse("1").unwrap();
    let draft_id = console.runtime.create_draft(config).unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();
    assert!(!preview.validation.valid);
    assert!(preview.validation.reason.is_some());
    assert!(preview.token.is_none());
    assert!(preview.batches.is_empty());
    assert!(preview.cells.is_empty());
    assert!(preview.immediate_fills.is_empty());

    let error = console
        .runtime
        .start_draft(&draft_id, "")
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Config(_)), "{error:?}");
    assert!(console.store.load_all_strategy_ids().unwrap().is_empty());
}
