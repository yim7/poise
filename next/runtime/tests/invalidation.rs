//! 观点失效价（第 7.5 节，B37–B41，B44 的相应部分）。

mod common;

use common::{console, core_harness, core_long_config};
use poise_runtime::RuntimeError;
use poise_strategy::config::{Invalidation, StrategyConfig};
use poise_strategy::units::{Price, Quantity, Rate};

fn invalidation(price: &str, remaining: &str) -> Invalidation {
    Invalidation {
        price: Price::parse(price).unwrap(),
        remaining: Quantity::parse(remaining).unwrap(),
        max_slippage: Rate::parse("0.01").unwrap(),
    }
}

fn core_long_with(invalidation: Option<Invalidation>) -> StrategyConfig {
    let mut config = core_long_config();
    config.long.as_mut().unwrap().invalidation = invalidation;
    config
}

/// B44：运行中修改失效价、剩余数量和滑点上限立即生效，记录一个新的参数版本；
/// 不合法的失效设置照样拒绝。
#[tokio::test]
async fn b44_the_invalidation_can_change_while_running() {
    let harness = core_harness().await;
    harness.step(0).await;

    let version = harness
        .runtime
        .update_config(
            &harness.strategy_id,
            core_long_with(Some(invalidation("70000", "0.3"))),
        )
        .unwrap();

    assert_eq!(version, 2);
    let stored = harness
        .runtime
        .store()
        .load_strategy(&harness.strategy_id)
        .unwrap();
    assert_eq!(
        stored.config.long.unwrap().invalidation,
        Some(invalidation("70000", "0.3"))
    );

    // 失效价落在范围里面：拒绝，版本不变。
    let error = harness
        .runtime
        .update_config(
            &harness.strategy_id,
            core_long_with(Some(invalidation("80000", "0.3"))),
        )
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Config(_)), "{error:?}");
    assert_eq!(
        harness
            .runtime
            .config_versions(&harness.strategy_id)
            .unwrap()
            .len(),
        2
    );
}

/// 第 7.3 节：设了失效价时，满仓压力测试的最不利价格是失效价。
#[tokio::test]
async fn the_stress_test_stops_at_the_invalidation_price() {
    let console = console();
    let draft_id = console
        .runtime
        .create_draft(core_long_with(Some(invalidation("70000", "0"))))
        .unwrap();

    let preview = console.runtime.preview_draft(&draft_id).await.unwrap();

    assert!(preview.validation.valid, "{:?}", preview.validation.reason);
    assert_eq!(
        preview.stress[0].adverse_price,
        Price::parse("70000").unwrap()
    );
}
