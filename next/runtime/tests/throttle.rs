//! 限速（第 4.1 节、第 9.4 节）：同一侧在 τ 时间内最多启动一个单位。
//!
//! 单位是批次（`r > 0`），格子跟随所属批次；放行时刻从上一个单位的第一笔**实际成交**
//! 算起，所以重启和暂停都不会多给额度。

mod common;

use chrono::Duration;
use common::{
    Harness, at, cheap_spec, core_long_config, core_only_config, harness_with, harness_with_spec,
    long_batch, long_cell, long_side, long_side_mut, long_state, neutral_grid_config, restart,
    short_side, throttled,
};
use poise_runtime::{ResumeOutcome, RuntimeError, Store};
use poise_strategy::batch::BatchIndex;
use poise_strategy::config::{SideConfig, StrategyConfig};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{OrderPurpose, OrderStatus, Unit};
use poise_strategy::planning::{Plan, WaitReason};
use poise_strategy::side::Direction;
use poise_strategy::units::{Position, Price, Quantity, Rate};

/// τ = 1 小时。
const HOUR: Duration = Duration::hours(1);

/// 两侧网格，只给多头侧限速：空头侧的单位一开始就全部放行。
fn long_throttled_neutral() -> StrategyConfig {
    StrategyConfig {
        long: Some(SideConfig {
            throttle_interval: Some(HOUR),
            ..long_side(&neutral_grid_config()).clone()
        }),
        ..neutral_grid_config()
    }
}

/// 在 [`long_throttled_neutral`] 之上再给空头侧加上限速。
fn both_sides_throttled_neutral() -> StrategyConfig {
    StrategyConfig {
        short: Some(SideConfig {
            throttle_interval: Some(HOUR),
            ..short_side(&neutral_grid_config()).clone()
        }),
        ..long_throttled_neutral()
    }
}

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
    let state = harness.runtime.state(&harness.strategy_id).unwrap();
    long_state(&state).throttle().released
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
    assert_eq!(planned(&report.plan), vec![long_batch(0)]);
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
        (0..5).map(long_cell).collect::<Vec<_>>()
    );
    assert!(!planned(&report.plan).contains(&long_batch(1)));
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
        (0..5).map(long_cell).collect::<Vec<_>>()
    );
    assert!(
        report
            .plan
            .place
            .iter()
            .all(|intent| intent.purpose == OrderPurpose::GridExit)
    );
    assert_eq!(report.position, Position::parse("0.25").unwrap());
    assert!(!planned(&report.plan).contains(&long_batch(1)));

    // 95 批的第一笔成交在 t = 10：满 τ 之前不放行 90 批。
    let report = harness.step(3609).await;
    assert!(
        !planned(&report.plan).contains(&long_batch(1)),
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
    assert!(planned(&report.plan).contains(&long_batch(1)));
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
    assert!(!planned(&report.plan).contains(&long_batch(2)));
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
    assert!(!planned(&report.plan).contains(&long_batch(1)));
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
    assert!(planned(&report.plan).contains(&long_batch(1)));
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
                intent.unit == long_cell(cell) && intent.purpose == OrderPurpose::GridEntry
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
        let sell = harness.working_cloid(cell, OrderPurpose::GridExit);
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
        assert_eq!(long_state(&state).throttle().first_fill_at, Some(at(10)));
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
    assert_eq!(long_state(&state).throttle().released, 1);
    assert_eq!(long_state(&state).throttle().first_fill_at, Some(at(10)));
    let report = harness.step(3609).await;
    assert!(!planned(&report.plan).contains(&long_batch(1)));
    let report = harness.step(3610).await;
    assert!(planned(&report.plan).contains(&long_batch(1)));
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
    assert!(!planned(&report.plan).contains(&long_batch(1)));
    let state = harness.runtime.state(&harness.strategy_id).unwrap();
    assert_eq!(long_state(&state).throttle().released, 1);
    assert_eq!(long_state(&state).throttle().first_fill_at, Some(at(10)));

    // 满 τ 之后才放行，中间的暂停不占时间也不额外给额度。
    let report = harness.step(3609).await;
    assert!(!planned(&report.plan).contains(&long_batch(1)));
    let report = harness.step(3610).await;
    assert!(planned(&report.plan).contains(&long_batch(1)));
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
    assert_eq!(long_side(&versions[0].config).throttle_interval, Some(HOUR));
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
        long_side(&versions[1].config).throttle_interval,
        Some(Duration::minutes(5))
    );
    assert_eq!(versions[1].recorded_at, at(11));

    // 按新间隔放行：95 批在 t = 10 成交，5 分钟之后（t = 310）就放行 90 批，
    // 换成原来的 1 小时则是 t = 3610。
    let report = harness.step(309).await;
    assert!(!planned(&report.plan).contains(&long_batch(1)));
    let report = harness.step(310).await;
    assert!(planned(&report.plan).contains(&long_batch(1)));
    assert_eq!(released(&harness), 2);

    // 其它参数运行中不能改：改了就得停下来按新配置重建。
    let mut range = core_long_config();
    long_side_mut(&mut range).price_low = Price::parse("74000").unwrap();
    assert_eq!(refused(&harness, range), "price_low");

    let mut spacing = core_long_config();
    long_side_mut(&mut spacing).grid_spacing = Price::parse("2000").unwrap();
    assert_eq!(refused(&harness, spacing), "grid_spacing");

    let mut position = core_long_config();
    long_side_mut(&mut position).max_position = Quantity::parse("2").unwrap();
    assert_eq!(refused(&harness, position), "max_position");

    let mut ratio = core_long_config();
    long_side_mut(&mut ratio).core_ratio = Rate::parse("0.6").unwrap();
    assert_eq!(refused(&harness, ratio), "core_ratio");

    let mut batches = core_long_config();
    long_side_mut(&mut batches).batch_count = Some(3);
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
        long_side(&versions[1].config).throttle_interval,
        Some(Duration::minutes(5))
    );
    let stored = harness.runtime.state(&harness.strategy_id).unwrap();
    assert_eq!(released(&harness), 2);
    assert_eq!(long_state(&stored).cells().len(), 20);
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
/// 反方向（不限速改成有限速）同样允许，限速从修改时刻重新计时。
#[tokio::test]
async fn b44_clearing_the_interval_while_running_lifts_the_throttle() {
    let harness = throttled_core();
    harness.step(0).await;

    harness
        .runtime
        .update_config(
            &harness.strategy_id,
            StrategyConfig {
                long: Some(SideConfig {
                    throttle_interval: None,
                    ..long_side(&core_long_config()).clone()
                }),
                ..core_long_config()
            },
        )
        .unwrap();

    let report = harness.step(10).await;
    assert_eq!(released(&harness), 4, "没有限速时所有单位都可以启动");
    assert!(planned(&report.plan).contains(&long_batch(1)));
    assert!(planned(&report.plan).contains(&long_batch(3)));
    assert!(
        !report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );
    // 去掉限速之后，状态跟一开始就不限速时一样：没有放行时刻。
    let state = harness.runtime.state(&harness.strategy_id).unwrap();
    assert_eq!(long_state(&state).throttle().released_at, None);
    assert_eq!(long_state(&state).throttle().first_fill_at, None);

    // 加回来也允许：放行进度按「修改这一刻」重建，而不是按启动时刻。
    harness.clock.set(at(100));
    let version = harness
        .runtime
        .update_config(&harness.strategy_id, throttled(core_long_config(), HOUR))
        .unwrap();
    assert_eq!(version, 3);
    assert_eq!(released(&harness), 1, "加回限速后退回只放行第一个单位");
    let state = harness.runtime.state(&harness.strategy_id).unwrap();
    assert_eq!(
        long_state(&state).throttle().first_fill_at,
        Some(at(100)),
        "计时从修改时刻开始"
    );
}

/// 运行中加上限速：允许，限速从修改时刻开始计时（第 4.1 节）。
///
/// 不限速时所有单位在启动时就已放行；加上限速后退回只放行第一个单位，其余单位重新
/// 按顺序放行。之前放行的单位在场上的订单留在原地，不会被撤掉——限速只决定「哪些单位
/// 可以挂新单」。
#[tokio::test]
async fn b44_adding_a_throttle_while_running_starts_the_interval_from_that_moment() {
    let harness = harness_with(core_long_config(), Store::open_in_memory().unwrap());
    assert_eq!(
        released(&harness),
        4,
        "不限速的策略在启动时就放行了全部单位"
    );

    // 起步：四批主仓都挂上买单。
    let report = harness.step(0).await;
    assert_eq!(
        planned(&report.plan),
        (0..4).map(long_batch).collect::<Vec<_>>()
    );
    let batch1 = harness.working_core_cloid(1);

    // 运行中加上限速：立刻生效，记第 2 个版本。
    harness.clock.set(at(100));
    let version = harness
        .runtime
        .update_config(&harness.strategy_id, throttled(core_long_config(), HOUR))
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(released(&harness), 1, "改成限速后退回只放行第一个单位");

    // 满 τ 就放行第二个单位，起点是修改时刻 at(100) 而不是起步时刻 at(0)：
    // 修改时刻之前的时间不算在间隔里，也不需要第一个单位先成交。
    let report = harness.step(3699).await;
    assert_eq!(released(&harness), 1);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );
    harness.step(3700).await;
    assert_eq!(released(&harness), 2);

    // 之前放行的批次在场上的订单还在原地：限速只挡住新单，不撤已有订单。
    assert!(harness.at_exchange(&batch1).is_some());
}

/// 运行中加上限速：已经开始的批次保持放行，后面的批次不会永久等待。
///
/// 场景：两批主仓已经成交结束，它们的流水也早已落在回看窗口之外（不会再有成交来
/// 「补记」）。按「退回只放行第一个单位」重建进度的话，第 1 批会被退回未放行——而它
/// 已经结束、不会再成交，第 2 批就永远等不到放行。
#[tokio::test]
async fn adding_a_throttle_keeps_finished_batches_released() {
    let harness = harness_with(core_only_config(), Store::open_in_memory().unwrap());
    harness.step(0).await;

    // 前两批主仓先后成交结束。
    for (batch, seconds) in [(0, 1), (1, 2)] {
        let cloid = harness.working_core_cloid(batch);
        harness
            .exchange
            .fill(&cloid, Quantity::parse("0.25").unwrap(), at(seconds))
            .unwrap();
        harness.step(seconds).await;
        let state = harness.runtime.state(&harness.strategy_id).unwrap();
        assert!(
            long_state(&state)
                .batch(BatchIndex(batch))
                .unwrap()
                .finished
        );
    }

    // 暂停再恢复：遗留挂单全部撤掉，恢复时按状态重新挂单。两笔成交的流水就此落在
    // 回查起点之后——还在场和还没对完账的订单都晚于它们。
    harness.pause(3).await;
    harness.resume(1000).await;
    let earliest = harness
        .orders()
        .iter()
        .filter(|order| order.record.is_working() || !order.fills_reconciled)
        .map(|order| order.placed_at)
        .min()
        .unwrap();
    assert_eq!(
        earliest,
        at(1000),
        "成交流水已经超出回看窗口，不会再被拉回来"
    );

    // 运行中加上限速：已经开始的第 0、1 批保持放行，第 2 批等修改时刻满 τ。
    harness.clock.set(at(1000));
    harness
        .runtime
        .update_config(&harness.strategy_id, throttled(core_only_config(), HOUR))
        .unwrap();
    assert_eq!(released(&harness), 2, "已经开始的批次保持放行");

    let report = harness.step(4599).await;
    assert_eq!(released(&harness), 2);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );
    harness.step(4600).await;
    assert_eq!(released(&harness), 3, "第 2 批在修改时刻满 τ 之后照常放行");

    // 第 2 批成交结束之后，第 3 批按它的成交时刻放行：链条继续往下走。
    let batch2 = harness.working_core_cloid(2);
    harness
        .exchange
        .fill(&batch2, Quantity::parse("0.25").unwrap(), at(4601))
        .unwrap();
    harness.step(4601).await;
    assert_eq!(released(&harness), 3);
    harness.step(8201).await;
    assert_eq!(released(&harness), 4);
}

/// 运行中加上限速：中间没开始的批次不挡住后面已经开始的批次。
///
/// 场景：不限速时第 0 批的主仓买单被交易所拒掉，第 1 批成交结束，两笔成交的流水都早已
/// 落在回看窗口之外。按「连续开始的前缀」重建进度的话，第 1 批会被退回未放行——它已经
/// 结束、不会再成交，满 τ 之后放行的是它自己，放行进度停在那里，第 2 批永远等不到。
#[tokio::test]
async fn adding_a_throttle_does_not_strand_batches_behind_a_gap() {
    let harness = harness_with(core_only_config(), Store::open_in_memory().unwrap());

    // 起步：第 0 批的主仓买单被交易所拒掉，它没有成交也没有结束。
    harness.exchange.reject_next_place("rejected");
    harness.step(0).await;
    let rejected = harness
        .orders()
        .into_iter()
        .find(|order| order.record.unit == long_batch(0))
        .expect("第 0 批的订单记录");
    assert_eq!(rejected.record.status, OrderStatus::Rejected);

    // 第 1 批主仓成交结束：放行顺序上出现了空洞（第 0 批没开始，第 1 批已结束）。
    let batch1 = harness.working_core_cloid(1);
    harness
        .exchange
        .fill(&batch1, Quantity::parse("0.25").unwrap(), at(1))
        .unwrap();
    harness.step(1).await;
    let state = harness.runtime.state(&harness.strategy_id).unwrap();
    assert!(long_state(&state).batch(BatchIndex(1)).unwrap().finished);
    assert_eq!(
        harness.core_filled(0),
        Quantity::ZERO,
        "第 0 批一直没有成交"
    );

    // 暂停再恢复：遗留挂单全部撤掉，恢复时按状态重新挂单，两笔成交的流水就此落在回查
    // 起点之后——还在场和还没对完账的订单都晚于它们。
    harness.pause(3).await;
    harness.resume(1000).await;
    let earliest = harness
        .orders()
        .iter()
        .filter(|order| order.record.is_working() || !order.fills_reconciled)
        .map(|order| order.placed_at)
        .min()
        .unwrap();
    assert_eq!(
        earliest,
        at(1000),
        "成交流水已经超出回看窗口，不会再被拉回来"
    );

    // 运行中加上限速：放行到最后一个已经开始的批次（第 1 批），第 0 批跟着一起放行。
    harness.clock.set(at(1000));
    harness
        .runtime
        .update_config(&harness.strategy_id, throttled(core_only_config(), HOUR))
        .unwrap();
    assert_eq!(released(&harness), 2, "放行到最后一个已经开始的批次");
    let state = harness.runtime.state(&harness.strategy_id).unwrap();
    assert_eq!(
        long_state(&state).throttle().first_fill_at,
        Some(at(1000)),
        "间隔的起点留着，不会被已经结束的批次吃掉"
    );

    // 满 τ 之前不放行：起点是修改时刻 at(1000)。
    let report = harness.step(4599).await;
    assert_eq!(released(&harness), 2);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::WaitingForThrottle)
    );

    // 满 τ 放行的是第 2 批，不是已经结束的第 1 批：链条继续往下走。
    harness.step(4600).await;
    assert_eq!(released(&harness), 3, "第 2 批在修改时刻满 τ 之后照常放行");
    let batch2 = harness.working_core_cloid(2);

    // 第 2 批成交结束之后，第 3 批按它的成交时刻放行。
    harness
        .exchange
        .fill(&batch2, Quantity::parse("0.25").unwrap(), at(4601))
        .unwrap();
    harness.step(4601).await;
    assert_eq!(released(&harness), 3);
    harness.step(8201).await;
    assert_eq!(released(&harness), 4);
}

/// 加上限速时，新配置和重建的放行进度写在同一个事务里。
///
/// 分成两次写，中间崩掉就留下「配置已经限速、放行进度还是全部放行」：重启后
/// `advance_throttle` 看到已放行的单位数已经等于单位总数就直接返回，限速静默失效，
/// 而且没有任何报错。所以配置写不进去时，进度也不能写进去。
#[tokio::test]
async fn adding_a_throttle_writes_the_config_and_the_progress_in_one_transaction() {
    let harness = harness_with(core_long_config(), Store::open_in_memory().unwrap());
    harness.step(0).await;
    harness.clock.set(at(100));
    // 让写策略状态失败，模拟磁盘写不进去。
    harness
        .runtime
        .store()
        .fail_state_writes_for_test()
        .unwrap();

    let error = harness
        .runtime
        .update_config(&harness.strategy_id, throttled(core_long_config(), HOUR));

    assert!(matches!(error, Err(RuntimeError::Store(_))));
    // 新配置和参数版本一起回滚：库里还是那套不限速的参数，下一步改配置才是第 2 版。
    let stored = harness
        .runtime
        .store()
        .load_strategy(&harness.strategy_id)
        .unwrap();
    assert!(long_side(&stored.config).throttle_interval.is_none());
    assert_eq!(
        harness
            .runtime
            .config_versions(&harness.strategy_id)
            .unwrap()
            .len(),
        1
    );
}

/// 运行中只给一侧加上限速：另一侧的放行进度和计时完全不动。
///
/// 另一侧没被改过。把两侧一起重建的话，它的放行进度会退回「最后一个已经开始的单位」
/// 并重新计时，下一次放行白白推后一整个 τ——它本来再等一会儿就该放行了。
#[tokio::test]
async fn adding_a_throttle_to_one_side_leaves_the_other_sides_progress_alone() {
    let harness = harness_with_spec(
        long_throttled_neutral(),
        cheap_spec(),
        Store::open_in_memory().unwrap(),
    );

    // 价格一路跌到 87：多头侧每满一个 τ 放行一格，这一格在自己的买价上当场买入，
    // 于是放行时刻和首笔成交时刻是同一个。
    for (second, price) in [(0, "89"), (3600, "88"), (7200, "87")] {
        harness
            .exchange
            .set_market_price(Price::parse(price).unwrap(), at(second));
        harness.step(second).await;
    }
    let long = harness.side_state(Direction::Long).throttle().clone();
    assert_eq!(long.released, 3);
    assert_eq!(long.released_at, Some(at(7200)));
    assert_eq!(long.first_fill_at, Some(at(7200)));
    // 空头侧不限速：五格一开始就全部放行。
    assert_eq!(
        harness.side_state(Direction::Short).throttle().released,
        5,
        "不限速的侧一开始就放行了全部单位"
    );

    // 运行中只给空头侧加上限速：多头侧没有被动过。
    harness.clock.set(at(10000));
    let version = harness
        .runtime
        .update_config(&harness.strategy_id, both_sides_throttled_neutral())
        .unwrap();
    assert_eq!(version, 2);

    assert_eq!(
        harness.side_state(Direction::Long).throttle(),
        &long,
        "另一侧的放行进度与计时保持不变"
    );
    let short = harness.side_state(Direction::Short).throttle().clone();
    assert_eq!(short.released, 1, "新加限速的一侧退回只放行第一个单位");
    assert_eq!(short.released_at, Some(at(10000)), "计时从修改时刻开始");
    assert_eq!(short.first_fill_at, Some(at(10000)));

    // 多头侧的计时接着原来那一次走：at(7200) 满 τ 是 at(10800)，不是 at(13600)。
    let report = harness.step(10800).await;
    assert_eq!(
        harness.side_state(Direction::Long).throttle().released,
        4,
        "多头侧的放行没有被推后"
    );
    assert!(
        planned(&report.plan).contains(&long_cell(3)),
        "多头侧新放行的格子照常挂单"
    );
    // 空头侧按新的计时走：at(10000) 满 τ 之前不放行第二个单位。
    assert_eq!(
        harness.side_state(Direction::Short).throttle().released,
        1,
        "空头侧按修改时刻计时"
    );
}

/// 限速的首笔成交时刻取真实成交时刻，不是当时用来兜底的那个估计值。
///
/// 成交记录可能比挂单状态晚到：撤销期间成交的那部分没有成交时间可用，只能先用「记账
/// 那一刻的时刻」兜底记下。流水到手之后必须改回真实成交时刻，否则放行会被推后——
/// 估计值总是晚于真实成交。
#[tokio::test]
async fn a_release_follows_the_real_fill_time_when_the_flow_arrives_late() {
    let harness = harness_with(
        throttled(core_only_config(), HOUR),
        Store::open_in_memory().unwrap(),
    );
    harness.step(0).await;
    let batch0 = harness.working_core_cloid(0);

    // 主仓在 at(50) 成交了 .1，但成交记录这一刻还查不到。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&batch0, Quantity::parse("0.1").unwrap(), at(50))
        .unwrap();

    // 暂停撤掉这张单：撤销前成交的那部分只能等下一步按挂单状态补记。
    harness.pause(100).await;
    harness.resume(101).await;

    // 补记用的是这一步的时刻 at(101)：挂单状态里没有成交时间。
    let throttle = |harness: &Harness| {
        let state = harness.runtime.state(&harness.strategy_id).unwrap();
        long_state(&state).throttle().clone()
    };
    assert_eq!(throttle(&harness).first_fill_at, Some(at(101)));

    // 流水到达：改回真实成交时刻 at(50)。
    harness.exchange.deliver_delayed_fills();
    harness.step(102).await;
    assert_eq!(
        throttle(&harness).first_fill_at,
        Some(at(50)),
        "放行时刻从真实成交算起，不是从兜底的那一刻算起"
    );

    // 满 τ 就放行第 1 批：起点是 at(50) 而不是 at(101)。
    harness.step(3649).await;
    assert_eq!(released(&harness), 1);
    harness.step(3650).await;
    assert_eq!(released(&harness), 2);
}
