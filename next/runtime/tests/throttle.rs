//! 限速（第 4.1 节、第 9.4 节）：同一侧在 τ 时间内最多启动一个单位。
//!
//! 单位是批次（`r > 0`），格子跟随所属批次；放行时刻从上一个单位的第一笔**实际成交**
//! 算起，所以重启和暂停都不会多给额度。

mod common;

use chrono::Duration;
use common::{Harness, at, core_long_config, harness_with, restart, throttled};
use poise_runtime::{ResumeOutcome, RuntimeError, Store};
use poise_strategy::batch::BatchIndex;
use poise_strategy::config::{SideConfig, StrategyConfig};
use poise_strategy::grid::CellIndex;
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{OrderPurpose, OrderStatus, Unit};
use poise_strategy::planning::{Plan, WaitReason};
use poise_strategy::units::{Position, Price, Quantity, Rate};

/// τ = 1 小时。
const HOUR: Duration = Duration::hours(1);

/// 四批主仓加网格，带限速。
fn throttled_core() -> Harness {
    harness_with(
        throttled(core_long_config(), HOUR),
        Store::open_in_memory().unwrap(),
    )
}

/// 这一步规划要挂单的单位，按规划顺序。
fn planned(plan: &Plan) -> Vec<Unit> {
    plan.place.iter().map(|intent| intent.unit).collect()
}

/// 这个策略的参数版本，按版本号从小到大。
fn versions(harness: &Harness) -> Vec<poise_runtime::StoredConfigVersion> {
    harness
        .runtime
        .config_versions(&harness.strategy_id)
        .unwrap()
}

fn released(harness: &Harness) -> usize {
    harness
        .runtime
        .state(&harness.strategy_id)
        .unwrap()
        .throttle()
        .released
}

/// 改配置被拒绝时，报出的是哪个字段。
fn refused(harness: &Harness, config: StrategyConfig) -> &'static str {
    match harness.runtime.update_config(&harness.strategy_id, config) {
        Err(RuntimeError::ConfigChangeNotAllowed { field }) => field,
        other => panic!("expected the change to be refused, got {other:?}"),
    }
}

/// B02：快跌时只放行一个批次，之后按限速依次放行；已启用的格子照常循环。
#[tokio::test]
async fn b02_a_crash_releases_one_batch_and_then_one_batch_per_interval() {
    let harness = throttled_core();

    // 起步：只有 95 批放行，它的主仓买单挂在批次价 95000 等待。
    let report = harness.step(0).await;
    assert_eq!(planned(&report.plan), vec![Unit::Batch(BatchIndex(0))]);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );
    let batch0 = harness.working_core_cloid(0);
    assert_eq!(
        harness.at_exchange(&batch0).unwrap().limit_price,
        Price::parse("95000").unwrap()
    );

    // 价格跌到 94900：95 批主仓全部成交，本批五格启用。限速不约束格子，
    // 它们照常在各自的买价挂单；已经穿过的 95 格当场买入。
    harness
        .exchange
        .set_market_price(Price::parse("94900").unwrap(), at(10));
    let report = harness.step(10).await;
    assert_eq!(harness.core_filled(0), Quantity::parse("0.175").unwrap());
    assert_eq!(
        planned(&report.plan),
        (0..5)
            .map(|cell| Unit::Cell(CellIndex(cell)))
            .collect::<Vec<_>>()
    );
    assert!(!planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );
    assert_eq!(harness.cell(0).to_buy_back, Quantity::ZERO, "95 格已买入");
    assert_eq!(
        harness.cell(1).to_buy_back,
        Quantity::parse("0.015").unwrap()
    );

    // 价格继续跌到 91000：94 到 91 四格依次买入，各自挂出卖单。
    harness
        .exchange
        .set_market_price(Price::parse("91000").unwrap(), at(100));
    let report = harness.step(100).await;
    assert_eq!(
        planned(&report.plan),
        (0..5)
            .map(|cell| Unit::Cell(CellIndex(cell)))
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .plan
            .place
            .iter()
            .all(|intent| intent.purpose == OrderPurpose::GridSell)
    );
    assert_eq!(report.position, Position::parse("0.25").unwrap());
    assert!(!planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));

    // 95 批的第一笔成交在 t = 10：满 τ 之前不放行 90 批。
    let report = harness.step(3609).await;
    assert!(
        !planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))),
        "放行时刻从实际成交时刻算起，不是从起步时刻"
    );
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );

    // t = 3610 放行 90 批：它的主仓买单挂在自己的批次价 90000。
    let report = harness.step(3610).await;
    assert!(planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    assert_eq!(released(&harness), 2);
    let batch1 = harness.working_core_cloid(1);
    assert_eq!(
        harness.at_exchange(&batch1).unwrap().limit_price,
        Price::parse("90000").unwrap()
    );
    // 90 批还没成交：85 批继续等。
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );
    let report = harness.step(100_000).await;
    assert!(!planned(&report.plan).contains(&Unit::Batch(BatchIndex(2))));
}

/// B03：下一批放行前价格反弹到批次价以上时，主仓买单挂在批次价等待，不追高。
#[tokio::test]
async fn b03_a_rebound_before_the_release_keeps_waiting_at_the_batch_price() {
    let harness = throttled_core();

    // 起步价 95500 已经在批次价 95000 之上：买单照样挂在 95000，不抬到市价。
    harness.step(0).await;
    let batch0 = harness.working_core_cloid(0);
    assert_eq!(
        harness.at_exchange(&batch0).unwrap().limit_price,
        Price::parse("95000").unwrap()
    );
    assert_eq!(
        harness.at_exchange(&batch0).unwrap().status,
        OrderStatus::Open
    );

    // 价格反弹到 96000 并走了很久：不追高，也不下新单。
    harness
        .exchange
        .set_market_price(Price::parse("96000").unwrap(), at(100));
    let report = harness.step(100).await;
    assert!(report.plan.place.is_empty());
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );
    assert_eq!(
        harness.at_exchange(&batch0).unwrap().limit_price,
        Price::parse("95000").unwrap(),
        "在场买单的限价不会被改到市价"
    );
    assert_eq!(harness.exchange.current_position(), Position::ZERO);

    // 95 批一直没有成交：靠前的单位没成交，后面的单位永远不放开。
    let report = harness.step(1_000_000).await;
    assert!(!planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    assert_eq!(released(&harness), 1);
}

/// B06：批次在更低价位才放行时，主仓在批次价成交，格子按启用规则立即买入。
#[tokio::test]
async fn b06_a_batch_released_below_its_price_buys_its_cells_immediately() {
    let harness = throttled_core();

    // 95 批买完，五格在 95500 的行情下挂着买单等待。
    harness.step(0).await;
    let batch0 = harness.working_core_cloid(0);
    harness
        .exchange
        .fill(&batch0, Quantity::parse("0.175").unwrap(), at(10))
        .unwrap();
    harness.step(10).await;

    // 60 分钟后放行 90 批，此时价格还在 95500：买单挂在批次价 90000 等待。
    let report = harness.step(3610).await;
    assert!(planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    let batch1 = harness.working_core_cloid(1);
    assert_eq!(
        harness.at_exchange(&batch1).unwrap().limit_price,
        Price::parse("90000").unwrap()
    );

    // 价格跌到 85000：90 批的买单在 90000 成交（更低价位才放行）。
    harness
        .exchange
        .set_market_price(Price::parse("85000").unwrap(), at(3620));
    let report = harness.step(3620).await;
    assert_eq!(harness.core_filled(1), Quantity::parse("0.175").unwrap());
    assert_eq!(
        harness.at_exchange(&batch1).unwrap().status,
        OrderStatus::Filled
    );
    // 90 到 86 五格启用，在 85 的行情下立即买入：买单当场成交，待买回量归零。
    // （95 批的五格在这一步换成卖单，它们和限速无关。）
    for cell in 5..10 {
        assert!(
            report.plan.place.iter().any(|intent| {
                intent.unit == Unit::Cell(CellIndex(cell))
                    && intent.purpose == OrderPurpose::GridBuy
            }),
            "第 {cell} 格必须挂买单"
        );
        assert!(harness.cell(cell).enabled);
        assert_eq!(
            harness.cell(cell).to_buy_back,
            Quantity::ZERO,
            "第 {cell} 格必须立即买入"
        );
    }
    // 这一步报出的仓位是挂单之前的核对结果：立即买入的数量在挂单过程中才记账，
    // 下一步的仓位核对看到的是买完之后的仓位（正好满仓 M = 1）。
    assert_eq!(report.position, Position::parse("0.425").unwrap());

    // 下一步按各自的卖价挂出卖单：91 到 87。
    let report = harness.step(3621).await;
    assert_eq!(report.position, Position::parse("0.5").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.5").unwrap());
    for (cell, sell_price) in (5..10).zip(["91000", "90000", "89000", "88000", "87000"]) {
        let sell = harness.working_cloid(cell, OrderPurpose::GridSell);
        assert_eq!(
            harness.at_exchange(&sell).unwrap().limit_price,
            Price::parse(sell_price).unwrap(),
            "第 {cell} 格的卖价"
        );
    }
    // 90 批虽然成交了，85 批仍要等它的第一笔成交之后满 τ。
    assert_eq!(released(&harness), 2);
}

/// 第 9.4 节：限速进度落库，重启之后按真实成交时刻继续。
#[tokio::test]
async fn a_restart_continues_the_interval_from_the_real_fill_time() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("poise.sqlite");

    let (strategy_id, exchange, batch0);
    {
        let harness = harness_with(
            throttled(core_long_config(), HOUR),
            Store::open(&path).unwrap(),
        );
        harness.step(0).await;
        batch0 = harness.working_core_cloid(0);
        // 部分成交：放行时刻从第一笔成交算起，不等这一批买完。
        harness
            .exchange
            .fill(&batch0, Quantity::parse("0.05").unwrap(), at(10))
            .unwrap();
        harness.step(10).await;
        let state = harness.runtime.state(&harness.strategy_id).unwrap();
        assert_eq!(state.throttle().first_fill_at, Some(at(10)));
        strategy_id = harness.strategy_id.clone();
        exchange = harness.exchange.clone();
        // 进程结束，数据库连接关闭。
    }

    let harness = restart(
        Store::open(&path).unwrap(),
        exchange.clone(),
        strategy_id.clone(),
    );

    // 重启读回的是成交时刻本身：还差一秒就不放行。
    let state = harness.runtime.state(&strategy_id).unwrap();
    assert_eq!(state.throttle().released, 1);
    assert_eq!(state.throttle().first_fill_at, Some(at(10)));
    let report = harness.step(3609).await;
    assert!(!planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    let report = harness.step(3610).await;
    assert!(planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
}

/// 暂停再恢复不额外给额度：恢复不会把 τ 重新计时。
#[tokio::test]
async fn pausing_and_resuming_does_not_grant_an_extra_release() {
    let harness = throttled_core();
    harness.step(0).await;
    let batch0 = harness.working_core_cloid(0);
    harness
        .exchange
        .fill(&batch0, Quantity::parse("0.175").unwrap(), at(10))
        .unwrap();
    harness.step(10).await;

    harness.pause(20).await;
    let outcome = harness.resume(100).await;
    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("expected the resumed step, got {outcome:?}");
    };

    // 恢复的这一步不会顺手放行下一个单位。
    assert!(!planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    let state = harness.runtime.state(&harness.strategy_id).unwrap();
    assert_eq!(state.throttle().released, 1);
    assert_eq!(state.throttle().first_fill_at, Some(at(10)));

    // 满 τ 之后才放行，中间的暂停不占时间也不额外给额度。
    let report = harness.step(3609).await;
    assert!(!planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    let report = harness.step(3610).await;
    assert!(planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    assert_eq!(released(&harness), 2);
}

/// B44：运行中修改限速间隔立即生效，并记一个新的参数版本。
#[tokio::test]
async fn b44_changing_the_throttle_interval_while_running_takes_effect_and_records_a_version() {
    let harness = throttled_core();
    harness.step(0).await;
    let batch0 = harness.working_core_cloid(0);
    harness
        .exchange
        .fill(&batch0, Quantity::parse("0.175").unwrap(), at(10))
        .unwrap();
    harness.step(10).await;

    // 建策略时记了第 1 个版本。
    let versions = harness
        .runtime
        .config_versions(&harness.strategy_id)
        .unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].version, 1);
    assert_eq!(versions[0].config.long.throttle_interval, Some(HOUR));
    assert_eq!(versions[0].recorded_at, at(0));

    // 改成 5 分钟：立刻生效，并记第 2 个版本。
    harness.clock.set(at(11));
    let version = harness
        .runtime
        .update_config(
            &harness.strategy_id,
            throttled(core_long_config(), Duration::minutes(5)),
        )
        .unwrap();
    assert_eq!(version, 2);

    let versions = harness
        .runtime
        .config_versions(&harness.strategy_id)
        .unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[1].version, 2);
    assert_eq!(
        versions[1].config.long.throttle_interval,
        Some(Duration::minutes(5))
    );
    assert_eq!(versions[1].recorded_at, at(11));

    // 按新间隔放行：95 批在 t = 10 成交，5 分钟之后（t = 310）就放行 90 批，
    // 换成原来的 1 小时则是 t = 3610。
    let report = harness.step(309).await;
    assert!(!planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    let report = harness.step(310).await;
    assert!(planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    assert_eq!(released(&harness), 2);

    // 其它参数运行中不能改：改了就得停下来按新配置重建。
    let mut range = core_long_config();
    range.long.price_low = Price::parse("74000").unwrap();
    assert_eq!(refused(&harness, range), "price_low");

    let mut spacing = core_long_config();
    spacing.long.grid_spacing = Price::parse("2000").unwrap();
    assert_eq!(refused(&harness, spacing), "grid_spacing");

    let mut position = core_long_config();
    position.long.max_position = Quantity::parse("2").unwrap();
    assert_eq!(refused(&harness, position), "max_position");

    let mut ratio = core_long_config();
    ratio.long.core_ratio = Rate::parse("0.6").unwrap();
    assert_eq!(refused(&harness, ratio), "core_ratio");

    let mut batches = core_long_config();
    batches.long.batch_count = Some(3);
    assert_eq!(refused(&harness, batches), "batch_count");

    // 换标的在规整时就拒绝了：新配置的标的必须和交易所的一致。
    let mut instrument = core_long_config();
    instrument.instrument = Instrument::try_new("ETH").unwrap();
    assert!(matches!(
        harness
            .runtime
            .update_config(&harness.strategy_id, instrument),
        Err(RuntimeError::Config(_))
    ));

    // 被拒绝的改动一点都不生效：版本和配置都还是改完限速间隔的那一份。
    let versions = harness
        .runtime
        .config_versions(&harness.strategy_id)
        .unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(
        versions[1].config.long.throttle_interval,
        Some(Duration::minutes(5))
    );
    let stored = harness.runtime.state(&harness.strategy_id).unwrap();
    assert_eq!(released(&harness), 2);
    assert_eq!(stored.cells().len(), 20);
}

/// B44：参数一个都没变时不记新版本，报当前版本号。
///
/// 参数版本是事后复盘「哪次改动让成交变了」的依据；同样的参数记两遍，版本号就不再
/// 对应不同的参数集（网页上连点两次保存就会走到这里）。
#[tokio::test]
async fn b44_saving_the_same_config_again_does_not_add_a_version() {
    let harness = throttled_core();
    harness.step(0).await;

    // 刚建好的配置再存一遍：还是第 1 版，不写新版本。
    let version = harness
        .runtime
        .update_config(&harness.strategy_id, throttled(core_long_config(), HOUR))
        .unwrap();
    assert_eq!(version, 1);
    assert_eq!(versions(&harness).len(), 1);

    // 改完限速间隔之后，同样的新配置再存一遍也不多出版本。
    harness.clock.set(at(11));
    let new_config = throttled(core_long_config(), Duration::minutes(5));
    let version = harness
        .runtime
        .update_config(&harness.strategy_id, new_config.clone())
        .unwrap();
    assert_eq!(version, 2);
    let version = harness
        .runtime
        .update_config(&harness.strategy_id, new_config)
        .unwrap();
    assert_eq!(version, 2, "同样的参数再存一遍还是第 2 版");
    assert_eq!(versions(&harness).len(), 2);
    assert_eq!(
        versions(&harness)[1].recorded_at,
        at(11),
        "不覆盖原版本的时刻"
    );
}

/// 把限速间隔改成「不限速」也是改同一个参数：剩下的单位立刻全部放行。
///
/// 这是本任务采取的读法——`None` 是限速间隔的取值之一，不是另一个参数。
/// 反方向（不限速改成有限速）只能停下来重建：不限速时所有单位在启动时就已放行。
#[tokio::test]
async fn b44_clearing_the_interval_while_running_lifts_the_throttle() {
    let harness = throttled_core();
    harness.step(0).await;

    harness
        .runtime
        .update_config(
            &harness.strategy_id,
            StrategyConfig {
                long: SideConfig {
                    throttle_interval: None,
                    ..core_long_config().long
                },
                ..core_long_config()
            },
        )
        .unwrap();

    let report = harness.step(10).await;
    assert_eq!(released(&harness), 4, "没有限速时所有单位都可以启动");
    assert!(planned(&report.plan).contains(&Unit::Batch(BatchIndex(1))));
    assert!(planned(&report.plan).contains(&Unit::Batch(BatchIndex(3))));
    assert!(
        !report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );

    // 取消限速之后再想加回来就不行了：放行进度已经无从重建。
    assert!(matches!(
        harness
            .runtime
            .update_config(&harness.strategy_id, throttled(core_long_config(), HOUR)),
        Err(RuntimeError::ThrottleCannotBeAdded)
    ));
}

/// 不限速的策略在运行中不能加限速：所有单位在启动时就已放行。
///
/// 不限速被编码成「全部放行」，而「已放行的单位不会退回未放行」是限速的前提，
/// 所以加限速只能停下来按新配置重建（第 2.4 节）。
#[tokio::test]
async fn b44_adding_a_throttle_to_an_unthrottled_strategy_is_refused() {
    let harness = harness_with(core_long_config(), Store::open_in_memory().unwrap());

    // 起步之前也一样：这一步还没有任何订单，但放行进度还是重建不出来。
    assert!(matches!(
        harness
            .runtime
            .update_config(&harness.strategy_id, throttled(core_long_config(), HOUR)),
        Err(RuntimeError::ThrottleCannotBeAdded)
    ));
    let versions = harness
        .runtime
        .config_versions(&harness.strategy_id)
        .unwrap();
    assert_eq!(versions.len(), 1, "被拒绝的改动不记版本");
    assert_eq!(
        released(&harness),
        4,
        "不限速的策略在启动时就放行了全部单位"
    );

    // 不走 `update_config` 就不会有这些麻烦：按新配置新建一个策略。
    let throttled_harness = throttled_core();
    let report = throttled_harness.step(0).await;
    assert_eq!(planned(&report.plan), vec![Unit::Batch(BatchIndex(0))]);
}
