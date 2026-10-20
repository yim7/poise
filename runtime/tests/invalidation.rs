//! 观点失效价（第 7.5 节，B37–B41，B44 的相应部分）。

mod common;

use common::{
    Harness, at, cheap_spec, console, core_harness, core_long_config, harness_with,
    harness_with_spec, neutral_grid_config,
};
use poise_runtime::{RuntimeError, Store, StoredOrder, StrategyStatus};
use poise_strategy::config::{Invalidation, StrategyConfig};
use poise_strategy::order::{OrderPurpose, OrderStatus, Side, Unit};
use poise_strategy::side::Direction;
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};

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

fn price(value: &str) -> Price {
    Price::parse(value).unwrap()
}

fn quantity(value: &str) -> Quantity {
    Quantity::parse(value).unwrap()
}

fn position(value: &str) -> Position {
    Position::parse(value).unwrap()
}

/// 做多主仓网格，失效价 70000、剩余数量 `remaining`、滑点上限 1%。
///
/// 第 0 秒按 95500 挂好四批主仓买单（格子要等所属批次开始成交才启用）；第 1 秒价格跌到
/// 69000：四批主仓 .7 全部成交，仓位 .7。
async fn crossed(remaining: &str) -> Harness {
    let harness = harness_with(
        core_long_with(Some(invalidation("70000", remaining))),
        Store::open_in_memory().unwrap(),
    )
    .await;
    harness.step(0).await;
    harness.exchange.set_market_price(price("69000"), at(1));
    assert_eq!(harness.exchange.current_position(), position("0.7"));
    harness
}

fn invalidation_orders(harness: &Harness) -> Vec<StoredOrder> {
    harness
        .orders()
        .into_iter()
        .filter(|order| order.record.purpose == OrderPurpose::Invalidation)
        .collect()
}

/// B37：跌破失效价后撤销全部订单，用只减仓单把仓位减到 R = .3；账上先减网格、再减主仓；
/// 所有订单有了结论之后进入已失效，价格回升也不再下单。
#[tokio::test]
async fn b37_a_long_grid_reduces_to_the_remaining_quantity_and_stays_invalidated() {
    let harness = crossed("0.3").await;

    let first = harness.step(1).await;

    assert_eq!(first.status, StrategyStatus::Invalidating);
    assert!(harness.working().is_empty(), "{:?}", harness.working());
    let sold = invalidation_orders(&harness);
    assert_eq!(sold.len(), 1);
    let order = &sold[0].record;
    assert_eq!(order.unit, Unit::Side(Direction::Long));
    assert_eq!(order.unit.side(order.purpose), Side::Sell);
    assert!(order.reduce_only);
    assert_eq!(order.quantity, quantity("0.4"));
    // 69000 × .99 = 68310：限价最多比标记价低 1%。
    assert_eq!(order.limit_price, price("68310"));
    assert_eq!(harness.exchange.current_position(), position("0.3"));

    let second = harness.step(2).await;
    assert_eq!(second.status, StrategyStatus::Invalidated);
    assert_eq!(second.position, position("0.3"));
    // 批次表按「先网格、后主仓」显示：主仓从最后一批减起，剩下第 0 批和第 1 批的一部分
    // （网格先减的顺序见策略的单元测试）。
    let batches = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .batches;
    let core: Vec<Quantity> = batches.iter().map(|batch| batch.core_filled).collect();
    assert_eq!(
        core,
        vec![
            quantity("0.175"),
            quantity("0.125"),
            Quantity::ZERO,
            Quantity::ZERO
        ]
    );
    assert!(
        harness
            .runtime
            .detail(&harness.strategy_id)
            .unwrap()
            .note
            .unwrap()
            .contains("已失效")
    );

    // 价格回来也不自动恢复：已失效的策略归档只读，驱动它会被拒绝。
    let placed = harness.exchange.place_count();
    harness.exchange.set_market_price(price("90000"), at(3));
    harness.clock.set(at(3));
    let error = harness
        .runtime
        .step(&harness.strategy_id)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Archived(_)), "{error:?}");
    assert!(
        !harness
            .runtime
            .drivable()
            .unwrap()
            .contains(&harness.strategy_id)
    );
    assert_eq!(harness.exchange.place_count(), placed);
}

/// 失效减仓卖出的部分算已实现：合计等于全部现金流加上剩余仓位按标记价的价值，均价只算
/// 剩下的那部分主仓。
#[tokio::test]
async fn the_reduction_counts_as_realized_in_the_total() {
    let harness = harness_with(
        core_long_with(Some(invalidation("70000", "0.3"))),
        Store::open_in_memory().unwrap(),
    )
    .await;
    // 手续费记为 0：这里只看归属，手续费的算法在策略的单元测试里。
    harness.exchange.set_fee_rate(Rate::ZERO);
    harness.step(0).await;
    harness.exchange.set_market_price(price("69000"), at(1));
    harness.step(1).await;
    harness.step(2).await;

    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let bought = detail.batches.iter().fold(Notional::ZERO, |total, batch| {
        total + Notional::of(batch.core_quantity, batch.price)
    });
    let held = detail.batches.iter().fold(Notional::ZERO, |total, batch| {
        total + Notional::of(batch.core_filled, batch.price)
    });
    // 四批主仓按批次价买进 .7，在 68310 卖出 .4，剩 .3 按 69000 计价。
    let sold = Notional::of(quantity("0.4"), price("68310"));
    let remaining = Notional::of(quantity("0.3"), price("69000"));

    assert_eq!(
        detail.reduction_realized_profit,
        Some(sold - (bought - held))
    );
    assert_eq!(detail.unrealized_pnl, remaining - held);
    assert_eq!(detail.total_pnl, remaining + sold - bought);
    // 四舍五入到最小报价单位。
    let units = i128::from(quantity("0.3").units());
    assert_eq!(
        detail.average_price,
        Some(Price::from_units(
            ((2 * held.raw() + units) / (2 * units)) as i64
        ))
    );
}

/// B38：滑点上限内没成交完，下一步按新的标记价继续，直到仓位不超过 R。
#[tokio::test]
async fn b38_an_unfilled_reduction_follows_the_new_mark_price() {
    let harness = crossed("0.3").await;
    harness.exchange.cap_next_ioc_fill(quantity("0.2"));

    harness.step(1).await;
    assert_eq!(harness.exchange.current_position(), position("0.5"));
    let first = &invalidation_orders(&harness)[0];
    // 没成交的部分当场撤销，不留在场上。
    assert_eq!(first.record.status, OrderStatus::Canceled);
    assert_eq!(first.record.cumulative_filled, quantity("0.2"));

    harness.exchange.set_market_price(price("68000"), at(2));
    let second = harness.step(2).await;
    assert_eq!(second.status, StrategyStatus::Invalidating);
    let orders = invalidation_orders(&harness);
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[1].record.quantity, quantity("0.2"));
    // 68000 × .99 = 67320：按新的标记价让出滑点上限。
    assert_eq!(orders[1].record.limit_price, price("67320"));
    assert_eq!(harness.exchange.current_position(), position("0.3"));

    assert_eq!(harness.step(3).await.status, StrategyStatus::Invalidated);
}

/// 开头那次单独读的标记价没读到，常规读取读到的已经越过失效价：同样当场进入失效，
/// 不按常规下单（第 7.5 节）。
#[tokio::test]
async fn the_regular_reading_also_triggers_the_invalidation() {
    let harness = crossed("0.3").await;
    harness.exchange.fail_next_mark_price("timeout");

    let report = harness.step(1).await;

    assert_eq!(report.status, StrategyStatus::Invalidating);
    let orders = harness.orders();
    let placed_after_start: Vec<_> = orders
        .iter()
        .filter(|order| order.placed_at == at(1))
        .map(|order| order.record.purpose)
        .collect();
    assert_eq!(placed_after_start, vec![OrderPurpose::Invalidation]);
    assert_eq!(harness.exchange.current_position(), position("0.3"));
}

/// 开头那次读标记价失败，常规读取读到了越界的标记价、随后可用资金读取失败：这一步不能
/// 因为别的读数失败就不止损，再看一次失效价，当场进入失效。
#[tokio::test]
async fn a_failed_reading_after_the_mark_still_triggers_the_invalidation() {
    let harness = crossed("0.3").await;
    harness.exchange.fail_next_mark_price("timeout");
    harness.account.fail_next_collateral("timeout");

    let report = harness.step(1).await;

    assert_eq!(report.status, StrategyStatus::Invalidating);
    assert_eq!(invalidation_orders(&harness).len(), 1);
    assert_eq!(harness.exchange.current_position(), position("0.3"));
}

/// 常规读取读到了越界的标记价、随后可用资金读取失败；补读的那一次又读不到，或者价格已经
/// 回到失效价之上：越界已经被看到过，照样进入失效，不能被后面的失败或回升抹掉。
#[tokio::test]
async fn an_observed_crossing_is_kept_when_the_rereading_fails_or_recovers() {
    for reread in [Err("timeout"), Ok(price("71000"))] {
        let harness = crossed("0.3").await;
        // 开头那次读不到；常规读取读到 69000；之后的补读按 `reread`。
        harness
            .exchange
            .script_mark_prices(vec![Err("timeout"), Ok(price("69000")), reread]);
        harness.account.fail_next_collateral("timeout");

        let report = harness.step(1).await;

        assert_eq!(report.status, StrategyStatus::Invalidating, "{reread:?}");
        assert_eq!(
            harness.runtime.status(&harness.strategy_id).unwrap(),
            StrategyStatus::Invalidating
        );
        // 失效中只会发只减仓单：没有新的增仓单。
        assert!(
            harness
                .orders()
                .iter()
                .filter(|order| order.placed_at == at(1))
                .all(|order| order.record.purpose == OrderPurpose::Invalidation),
            "{reread:?}"
        );
        // 下一步按交易所的行情把仓位减到 R。
        harness.step(2).await;
        assert_eq!(
            harness.exchange.current_position(),
            position("0.3"),
            "{reread:?}"
        );
    }
}

/// 常规读取读到的标记价收到时新鲜、已经越过失效价；之后的可用资金查询拖了 15 秒（超过
/// 行情过期时间）然后失败，补读又回到失效价之上：越界在拿到标记价的那一刻就算数，照样
/// 进入失效，后面的读取根本不做。
#[tokio::test]
async fn a_crossing_counts_when_it_is_read_even_if_later_reads_drag_on() {
    let harness = crossed("0.3").await;
    harness.exchange.script_mark_prices(vec![
        Err("timeout"),
        Ok(price("69000")),
        Ok(price("71000")),
    ]);
    harness
        .account
        .slow_next_collateral(harness.clock.clone(), chrono::Duration::seconds(15));
    harness.account.fail_next_collateral("timeout");

    let report = harness.step(1).await;

    assert_eq!(report.status, StrategyStatus::Invalidating);
    assert!(
        harness
            .orders()
            .iter()
            .filter(|order| order.placed_at >= at(1))
            .all(|order| order.record.purpose == OrderPurpose::Invalidation)
    );
}

/// 收场时行情已经过期：按旧标记价得出的「仓位已经减到 R」不算数，留在失效中，下一步
/// 重读之后再收场（第 7.2 节）。
#[tokio::test]
async fn a_stale_mark_price_does_not_finish_the_invalidation() {
    let harness = crossed("0.3").await;
    harness.step(1).await;
    // 标记价的响应在路上耽搁了 11 秒，超过行情过期时间 10 秒。
    harness
        .exchange
        .slow_mark_price(harness.clock.clone(), chrono::Duration::seconds(11));

    let stale = harness.step(2).await;

    assert_eq!(stale.status, StrategyStatus::Invalidating);
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap();
    assert!(note.contains("行情过期"), "{note}");

    harness
        .exchange
        .slow_mark_price(harness.clock.clone(), chrono::Duration::zero());
    assert_eq!(harness.step(30).await.status, StrategyStatus::Invalidated);
}

/// B39：失效触发时成交同步失败，减仓照常执行，数量按交易所实时查询的仓位算。成交对完账
/// 之前不算完成。
#[tokio::test]
async fn b39_the_reduction_runs_while_the_fills_are_unavailable() {
    let harness = crossed("0.3").await;
    harness
        .exchange
        .fail_fills_since(1, "fills feed disconnected");

    let first = harness.step(1).await;

    assert_eq!(first.status, StrategyStatus::Invalidating);
    assert_eq!(invalidation_orders(&harness).len(), 1);
    assert_eq!(harness.exchange.current_position(), position("0.3"));

    assert_eq!(harness.step(2).await.status, StrategyStatus::Invalidated);
}

/// B39b：已有仓位；一张增仓单的请求在路上（结果未知）；失效触发、R = 0，平掉现有仓位；
/// 之后旧增仓单落地成交，仓位变成它的数量：继续减仓到 0，增仓单有了结论且仓位为 0 之后
/// 才进入已失效。
#[tokio::test]
async fn b39b_a_late_increasing_fill_is_reduced_again() {
    let harness = harness_with(
        core_long_with(Some(invalidation("70000", "0"))),
        Store::open_in_memory().unwrap(),
    )
    .await;
    harness.step(0).await;
    // 第 1 秒跌到 89000：前两批主仓成交，下一步要挂出新启用格子的单。
    harness.exchange.set_market_price(price("89000"), at(1));
    harness.exchange.hold_next_increasing_place();
    harness.step(1).await;
    let in_flight = harness
        .orders()
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Unknown)
        .expect("an increasing order is in flight");
    assert!(!in_flight.record.reduce_only);

    harness.exchange.set_market_price(price("69000"), at(2));
    let first = harness.step(2).await;
    assert_eq!(first.status, StrategyStatus::Invalidating);
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    // 仓位已经是 0，但增仓单还没有结论：不算完成。
    assert_eq!(harness.step(3).await.status, StrategyStatus::Invalidating);

    // 第 4 秒旧请求到了交易所（还没过 expiresAfter），当场按 69000 成交。
    harness.exchange.deliver_held_places(at(4));
    let late = in_flight.record.quantity;
    assert_eq!(
        harness.exchange.current_position(),
        Position::from_units(late.units() as i64)
    );

    let again = harness.step(5).await;
    assert_eq!(again.status, StrategyStatus::Invalidating);
    let orders = invalidation_orders(&harness);
    assert_eq!(orders.last().unwrap().record.quantity, late);
    assert_eq!(harness.exchange.current_position(), Position::ZERO);

    assert_eq!(harness.step(6).await.status, StrategyStatus::Invalidated);
}

/// B39c：持仓 .7、R = .3；减仓单卖 .4 的请求在路上、结果未知：补发数量 = .7 − .3 − .4 = 0，
/// 不再补单；它有了结论之后再按实际仓位决定，不会卖过 R。
#[tokio::test]
async fn b39c_a_reduction_with_an_unknown_result_is_not_sent_again() {
    let harness = crossed("0.3").await;
    harness.exchange.hold_next_reducing_place();

    harness.step(1).await;
    let orders = invalidation_orders(&harness);
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].record.status, OrderStatus::Unknown);
    let placed = harness.exchange.place_count();

    let waiting = harness.step(2).await;
    assert_eq!(waiting.status, StrategyStatus::Invalidating);
    assert_eq!(harness.exchange.place_count(), placed);

    // 请求到了，卖出 .4：仓位正好是 R，不会再卖。
    harness.exchange.deliver_held_places(at(3));
    assert_eq!(harness.exchange.current_position(), position("0.3"));
    assert_eq!(harness.step(4).await.status, StrategyStatus::Invalidated);
    assert_eq!(invalidation_orders(&harness).len(), 1);
    assert_eq!(harness.exchange.current_position(), position("0.3"));
}

/// B39d：全部订单已终态，仓位超出 R 的部分不足最低下单金额：视为失效完成，说明里写出尾量。
#[tokio::test]
async fn b39d_a_tail_below_the_minimum_order_finishes_the_invalidation() {
    let harness = crossed("0.3").await;
    harness.step(1).await;
    // 账户上多出 .0005（不足一个数量步长 .001），比如用户手动加了一点。
    harness.exchange.set_position(position("0.3005"));

    let report = harness.step(2).await;

    assert_eq!(report.status, StrategyStatus::Invalidated);
    let note = harness
        .runtime
        .detail(&harness.strategy_id)
        .unwrap()
        .note
        .unwrap();
    assert!(note.contains("0.0005"), "{note}");
    assert!(note.contains("不足最低下单金额"), "{note}");
}

/// B40：中性网格任一侧失效时全部平仓，策略进入已失效。
#[tokio::test]
async fn b40_a_neutral_grid_flattens_when_either_side_is_invalidated() {
    let mut config = neutral_grid_config();
    config.short.as_mut().unwrap().invalidation = Some(Invalidation {
        price: price("97"),
        remaining: Quantity::ZERO,
        max_slippage: Rate::parse("0.01").unwrap(),
    });
    let harness = harness_with_spec(config, cheap_spec(), Store::open_in_memory().unwrap()).await;
    harness.exchange.set_market_price(price("90"), at(0));
    harness.step(0).await;
    // 涨到 98：空头侧五格全部开空，净仓位 −.25。
    harness.exchange.set_market_price(price("98"), at(1));
    assert_eq!(
        harness.exchange.current_position(),
        Position::from_units(-quantity("0.25").units())
    );

    let first = harness.step(1).await;

    assert_eq!(first.status, StrategyStatus::Invalidating);
    assert!(harness.working().is_empty(), "{:?}", harness.working());
    let order = &invalidation_orders(&harness)[0].record;
    assert_eq!(order.unit, Unit::Side(Direction::Short));
    assert_eq!(order.unit.side(order.purpose), Side::Buy);
    assert_eq!(order.quantity, quantity("0.25"));
    assert_eq!(harness.exchange.current_position(), Position::ZERO);

    assert_eq!(harness.step(2).await.status, StrategyStatus::Invalidated);
}

/// 失效中不能暂停、恢复、改参数：它正在减仓。停止可以，撤单之后归档。
#[tokio::test]
async fn an_invalidating_strategy_can_only_be_stopped() {
    let harness = crossed("0.3").await;
    harness.exchange.cap_next_ioc_fill(quantity("0.2"));
    assert_eq!(harness.step(1).await.status, StrategyStatus::Invalidating);

    assert!(matches!(
        harness.runtime.pause(&harness.strategy_id).await,
        Err(RuntimeError::NotRunning(..))
    ));
    assert!(matches!(
        harness.runtime.resume(&harness.strategy_id).await,
        Err(RuntimeError::NotRunning(..))
    ));
    assert!(matches!(
        harness
            .runtime
            .update_config(&harness.strategy_id, core_long_with(None)),
        Err(RuntimeError::NotRunning(..))
    ));

    harness.stop(2).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopped
    );
    // 停止不再减仓：剩下的仓位由用户处理。
    assert_eq!(harness.exchange.current_position(), position("0.5"));
}

/// 进程重启不打断失效：失效中的策略不落「恢复中」，常驻循环接着驱动它减仓。
#[tokio::test]
async fn an_invalidating_strategy_keeps_reducing_after_a_restart() {
    let harness = crossed("0.3").await;
    harness.exchange.cap_next_ioc_fill(quantity("0.2"));
    harness.step(1).await;

    assert!(
        !harness
            .runtime
            .to_restore()
            .unwrap()
            .contains(&harness.strategy_id)
    );
    assert!(
        harness
            .runtime
            .drivable()
            .unwrap()
            .contains(&harness.strategy_id)
    );
}

/// 已暂停的策略越过失效价同样减仓：失效价是止损，暂停只是先别做网格（第 7.5 节）。
#[tokio::test]
async fn a_paused_strategy_is_still_invalidated() {
    let harness = crossed("0.3").await;
    harness.pause(1).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
    assert!(
        harness
            .runtime
            .drivable()
            .unwrap()
            .contains(&harness.strategy_id)
    );

    assert_eq!(harness.step(2).await.status, StrategyStatus::Invalidating);
    assert_eq!(harness.exchange.current_position(), position("0.3"));
    assert_eq!(harness.step(3).await.status, StrategyStatus::Invalidated);
}

/// 挂好单之后点暂停或停止，一张撤单结果不明：状态停在暂停中或停止中。之后价格跌到 69000。
async fn interrupted(stop: bool) -> Harness {
    let harness = harness_with(
        core_long_with(Some(invalidation("70000", "0.3"))),
        Store::open_in_memory().unwrap(),
    )
    .await;
    harness.step(0).await;
    harness.exchange.unknown_next_cancel("timed out");
    if stop {
        harness.stop(0).await;
    } else {
        harness.pause(0).await;
    }
    harness.exchange.set_market_price(price("69000"), at(1));
    harness
}

/// 暂停中（撤单还没确认）、恢复中（进程重启后）的策略越过失效价也进入失效。
#[tokio::test]
async fn pausing_and_restoring_strategies_are_invalidated_too() {
    let pausing = interrupted(false).await;
    assert_eq!(
        pausing.runtime.status(&pausing.strategy_id).unwrap(),
        StrategyStatus::Pausing
    );
    // 暂停时只剩一张没撤掉的买单，跌下来成交 .175，本来就不超过 R：这一步直接收场。
    let status = pausing.step(2).await.status;
    assert!(
        matches!(
            status,
            StrategyStatus::Invalidating | StrategyStatus::Invalidated
        ),
        "{status:?}"
    );

    let restoring = crossed("0.3").await;
    let report = restoring.recover(1).await;
    assert_eq!(report.status, StrategyStatus::Invalidating);
    assert_eq!(restoring.exchange.current_position(), position("0.3"));
}

/// 停止中不进入失效：点了停止就是用户决定自己接管仓位。
#[tokio::test]
async fn a_stopping_strategy_is_not_invalidated() {
    let harness = interrupted(true).await;
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Stopping
    );
    let held = harness.exchange.current_position();

    let report = harness.step(2).await;

    assert_eq!(report.status, StrategyStatus::Stopped);
    assert!(invalidation_orders(&harness).is_empty());
    assert_eq!(harness.exchange.current_position(), held);
}
