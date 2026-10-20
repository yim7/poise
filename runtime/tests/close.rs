//! 市价平仓（第 7.7 节，B49–B58 的后端部分）。

mod common;

use common::{Harness, at, console, core_harness, long_grid_config};
use poise_runtime::{RuntimeError, StoredOrder, StrategyStatus};
use poise_strategy::order::{OrderPurpose, OrderStatus, Side, Unit};
use poise_strategy::side::Direction;
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};

fn price(value: &str) -> Price {
    Price::parse(value).unwrap()
}

fn quantity(value: &str) -> Quantity {
    Quantity::parse(value).unwrap()
}

/// 做多主仓网格：第 0 秒挂好单，第 1 秒价格跌到 89000，前两批主仓和一部分格子成交，
/// 网格还挂着单。
async fn holding() -> Harness {
    let harness = core_harness().await;
    harness.step(0).await;
    harness.exchange.set_market_price(price("89000"), at(1));
    harness.step(1).await;
    assert!(harness.exchange.current_position() > Position::ZERO);
    assert!(!harness.working().is_empty());
    harness
}

fn close_orders(harness: &Harness) -> Vec<StoredOrder> {
    harness
        .orders()
        .into_iter()
        .filter(|order| order.record.purpose == OrderPurpose::Close)
        .collect()
}

fn status(harness: &Harness) -> StrategyStatus {
    harness
        .runtime
        .store()
        .load_strategy(&harness.strategy_id)
        .unwrap()
        .status
}

/// B49：停止时选市价平仓：先撤单收尾，再进入平仓中；只减仓 IOC 单按标记价让出 5% 平掉
/// 标的净仓位；仓位为 0、订单都有结论之后归档为已平仓。
#[tokio::test]
async fn b49_stopping_with_a_market_close_cancels_first_then_closes_the_position() {
    let harness = holding().await;
    let held = harness.exchange.current_position();

    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();

    // 撤单收尾在前：这一步只撤单，不发平仓单。
    assert!(harness.working().is_empty(), "{:?}", harness.working());
    assert!(close_orders(&harness).is_empty());
    assert_eq!(status(&harness), StrategyStatus::Closing);
    assert!(
        harness
            .runtime
            .drivable()
            .unwrap()
            .contains(&harness.strategy_id)
    );

    let first = harness.step(3).await;
    assert_eq!(first.status, StrategyStatus::Closing);
    let closes = close_orders(&harness);
    assert_eq!(closes.len(), 1);
    let order = &closes[0].record;
    assert_eq!(order.unit, Unit::Side(Direction::Long));
    assert_eq!(order.unit.side(order.purpose), Side::Sell);
    assert!(order.reduce_only);
    assert_eq!(order.quantity, Quantity::from_units(held.units()));
    // 89000 × .95 = 84550：限价最多比标记价低 5%。
    assert_eq!(order.limit_price, price("84550"));
    assert_eq!(harness.exchange.current_position(), Position::ZERO);

    let second = harness.step(4).await;
    assert_eq!(second.status, StrategyStatus::Closed);
    assert!(status(&harness).is_archived());
    assert!(
        harness
            .runtime
            .detail(&harness.strategy_id)
            .unwrap()
            .note
            .unwrap()
            .contains("已平仓")
    );
    // 归档之后不再驱动。
    assert!(
        !harness
            .runtime
            .drivable()
            .unwrap()
            .contains(&harness.strategy_id)
    );
    let error = harness
        .runtime
        .step(&harness.strategy_id)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Archived(_)), "{error:?}");
}

/// B50：普通停止不平仓，和 B45 一样。
#[tokio::test]
async fn b50_a_plain_stop_keeps_the_position() {
    let harness = holding().await;
    let held = harness.exchange.current_position();

    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();

    assert_eq!(status(&harness), StrategyStatus::Stopped);
    assert!(close_orders(&harness).is_empty());
    assert_eq!(harness.exchange.current_position(), held);
}

/// B51：5% 以内没成交完，下一步按新的标记价接着发，直到仓位为 0。
#[tokio::test]
async fn b51_an_unfinished_close_continues_at_the_new_mark_price() {
    let harness = holding().await;
    let held = harness.exchange.current_position();
    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();

    harness.exchange.cap_next_ioc_fill(quantity("0.1"));
    harness.step(3).await;
    assert_eq!(
        harness.exchange.current_position(),
        Position::from_units(held.units() - quantity("0.1").units())
    );

    harness.exchange.set_market_price(price("88000"), at(4));
    let report = harness.step(4).await;
    assert_eq!(report.status, StrategyStatus::Closing);
    let closes = close_orders(&harness);
    assert_eq!(closes.len(), 2);
    assert_eq!(
        closes[1].record.quantity,
        Quantity::from_units(held.units() - quantity("0.1").units())
    );
    // 88000 × .95 = 83600。
    assert_eq!(closes[1].record.limit_price, price("83600"));
    assert_eq!(harness.exchange.current_position(), Position::ZERO);

    assert_eq!(harness.step(5).await.status, StrategyStatus::Closed);
}

/// B52：停止时还在场的买单在撤单之前又成交了一部分：平仓数量按收尾之后的实时净仓位算。
#[tokio::test]
async fn b52_the_close_uses_the_position_after_the_cancellations_settle() {
    let harness = holding().await;
    let held = harness.exchange.current_position();
    let buy = harness
        .working()
        .into_iter()
        .find(|order| order.record.unit.side(order.record.purpose) == Side::Buy)
        .expect("a working buy order");
    harness.exchange.fill_before_next_cancel(
        1,
        buy.record.client_order_id.clone(),
        quantity("0.01"),
        at(2),
    );

    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();
    harness.step(3).await;

    let closes = close_orders(&harness);
    assert_eq!(closes.len(), 1);
    assert_eq!(
        closes[0].record.quantity,
        Quantity::from_units(held.units() + quantity("0.01").units())
    );
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
}

/// B53：已停止、标的上仍有仓位：可以市价平仓，确认后进入平仓中，平完归档为已平仓。
#[tokio::test]
async fn b53_an_archived_strategy_with_a_position_can_be_closed() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    assert_eq!(status(&harness), StrategyStatus::Stopped);

    harness.clock.set(at(10));
    let next = harness.runtime.close(&harness.strategy_id).await.unwrap();

    assert_eq!(next, StrategyStatus::Closing);
    assert_eq!(status(&harness), StrategyStatus::Closing);
    harness.step(11).await;
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    assert_eq!(harness.step(12).await.status, StrategyStatus::Closed);
}

/// B53：停止中点平仓，等于补上平仓标记：收尾之后进入平仓中。
#[tokio::test]
async fn b53_closing_while_stopping_adds_the_close_to_the_stop() {
    let harness = holding().await;
    // 撤单结果未知：停止留在停止中。
    harness.exchange.unknown_next_cancels(20, "timeout");
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    assert_eq!(status(&harness), StrategyStatus::Stopping);

    let next = harness.runtime.close(&harness.strategy_id).await.unwrap();
    assert_eq!(next, StrategyStatus::Stopping);
    assert!(close_orders(&harness).is_empty());
}

/// B53：标的上有别的未归档策略时拒绝平仓：仓位归它管。
#[tokio::test]
async fn b53_a_close_is_refused_while_another_strategy_is_live_on_the_instrument() {
    let console = console();
    let first = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&first).await.unwrap();
    console
        .runtime
        .start_draft(&first, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console
        .exchange
        .set_position(Position::parse("0.05").unwrap());
    console.clock.set(at(1));
    console.runtime.stop(&first).await.unwrap();

    let second = console.runtime.create_draft(long_grid_config()).unwrap();
    console.clock.set(at(2));
    let preview = console.runtime.preview_draft(&second).await.unwrap();
    console
        .runtime
        .start_draft(&second, preview.token.as_deref().unwrap())
        .await
        .unwrap();

    let error = console.runtime.close(&first).await.unwrap_err();
    match error {
        RuntimeError::InstrumentBusy { strategy_id, .. } => assert_ne!(strategy_id, first),
        other => panic!("expected InstrumentBusy, got {other:?}"),
    }
}

/// B53：净仓位按标记价不足最低下单金额时没有可平的，拒绝。
#[tokio::test]
async fn b53_nothing_to_close_is_refused() {
    let harness = core_harness().await;
    harness.step(0).await;
    harness.clock.set(at(1));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    assert_eq!(harness.exchange.current_position(), Position::ZERO);

    let error = harness
        .runtime
        .close(&harness.strategy_id)
        .await
        .unwrap_err();

    assert!(
        matches!(error, RuntimeError::NothingToClose(_)),
        "{error:?}"
    );
    assert_eq!(status(&harness), StrategyStatus::Stopped);
}

/// B54：平仓中的标的再启动一个新策略：拒绝（同 B46）。
#[tokio::test]
async fn b54_a_closing_strategy_keeps_the_instrument_busy() {
    let console = console();
    let first = console.runtime.create_draft(long_grid_config()).unwrap();
    let preview = console.runtime.preview_draft(&first).await.unwrap();
    console
        .runtime
        .start_draft(&first, preview.token.as_deref().unwrap())
        .await
        .unwrap();
    console
        .exchange
        .set_position(Position::parse("0.05").unwrap());
    console.clock.set(at(1));
    console.runtime.stop_and_close(&first).await.unwrap();
    assert_eq!(
        console.store.load_strategy(&first).unwrap().status,
        StrategyStatus::Closing
    );

    let second = console.runtime.create_draft(long_grid_config()).unwrap();
    console.clock.set(at(2));
    let preview = console.runtime.preview_draft(&second).await.unwrap();
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

/// B55：平仓单结果未知：补发数量扣掉它的余量，不重复平仓。
#[tokio::test]
async fn b55_an_unknown_close_order_is_not_sent_again() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();

    harness.exchange.unknown_next_place("timeout");
    harness.step(3).await;
    let first = close_orders(&harness);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].record.status, OrderStatus::Unknown);

    harness.step(4).await;
    assert_eq!(close_orders(&harness).len(), 1);
    assert_eq!(status(&harness), StrategyStatus::Closing);
}

/// B56：平仓中停止：放弃剩下的平仓，收尾后归档为已停止，剩余仓位保留。
#[tokio::test]
async fn b56_stopping_while_closing_gives_up_the_rest_of_the_close() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();
    harness.exchange.cap_next_ioc_fill(quantity("0.1"));
    harness.step(3).await;
    let left = harness.exchange.current_position();
    assert!(left > Position::ZERO);

    harness.clock.set(at(4));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();

    assert_eq!(status(&harness), StrategyStatus::Stopped);
    assert_eq!(harness.exchange.current_position(), left);
    harness.clock.set(at(5));
    assert!(
        !harness
            .runtime
            .drivable()
            .unwrap()
            .contains(&harness.strategy_id)
    );
}

/// B57：平仓中重启：不进入恢复中，重启后接着平。
#[tokio::test]
async fn b57_a_restart_does_not_interrupt_the_close() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();

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

fn held(harness: &Harness) -> Option<poise_runtime::HeldPosition> {
    harness
        .runtime
        .strategies()
        .unwrap()
        .into_iter()
        .find(|summary| summary.strategy_id == harness.strategy_id)
        .unwrap()
        .held
}

/// B59：停止时保留了仓位：读一次归档标的的仓位之后，列表上显示仍有仓位；在交易所上
/// 手动平掉一半，下一次读数后显示剩下的一半；全部平掉后不再列出。
#[tokio::test]
async fn b59_an_archived_strategy_shows_the_position_still_held() {
    let harness = holding().await;
    let position = harness.exchange.current_position();
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    // 还没读过：不按归档时的旧快照说话。
    assert_eq!(held(&harness), None);

    harness.clock.set(at(10));
    harness.runtime.read_archived_positions().await.unwrap();
    let first = held(&harness).unwrap();
    assert_eq!(first.position, position);
    assert_eq!(first.read_at, at(10));
    assert_eq!(first.market_price, price("89000"));

    let half = Position::from_units(position.units() / 2);
    harness.exchange.set_position(half);
    harness.clock.set(at(70));
    harness.runtime.read_archived_positions().await.unwrap();
    assert_eq!(held(&harness).unwrap().position, half);

    harness.exchange.set_position(Position::ZERO);
    harness.clock.set(at(130));
    harness.runtime.read_archived_positions().await.unwrap();
    assert_eq!(held(&harness), None);
}

/// B60：同一标的有两份已归档策略时只算一处，挂在最近归档的那一份上；标的上有运行中的
/// 策略时不列出，仓位归它管。
#[tokio::test]
async fn b60_the_position_belongs_to_the_latest_archived_strategy_or_to_a_live_one() {
    let console = console();
    let start = |config| {
        let runtime = console.runtime.clone();
        async move {
            let draft = runtime.create_draft(config).unwrap();
            let preview = runtime.preview_draft(&draft).await.unwrap();
            runtime
                .start_draft(&draft, preview.token.as_deref().unwrap())
                .await
                .unwrap();
            draft
        }
    };
    let held_of = |strategy_id: &poise_runtime::StrategyId| {
        console
            .runtime
            .strategies()
            .unwrap()
            .into_iter()
            .find(|summary| &summary.strategy_id == strategy_id)
            .unwrap()
            .held
    };
    let first = start(long_grid_config()).await;
    console
        .exchange
        .set_position(Position::parse("0.05").unwrap());
    console.clock.set(at(1));
    console.runtime.stop(&first).await.unwrap();
    console.clock.set(at(2));
    let second = start(long_grid_config()).await;
    console.clock.set(at(3));
    console.runtime.stop(&second).await.unwrap();

    console.clock.set(at(10));
    console.runtime.read_archived_positions().await.unwrap();
    assert_eq!(held_of(&first), None);
    assert_eq!(
        held_of(&second).unwrap().position,
        Position::parse("0.05").unwrap()
    );

    console.clock.set(at(20));
    let third = start(long_grid_config()).await;
    // 新策略一启动仓位就归它管：不等下一次读数，旧的那份马上不再列出。
    assert_eq!(held_of(&second), None);
    console.clock.set(at(30));
    console.runtime.read_archived_positions().await.unwrap();
    assert_eq!(held_of(&first), None);
    assert_eq!(held_of(&second), None);
    assert_eq!(held_of(&third), None);
}

/// B61：读不到时沿用上一次的读数；超过 5 分钟没读到新的就不再显示，也不给平仓。
#[tokio::test]
async fn b61_a_reading_older_than_five_minutes_is_not_shown() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    harness.clock.set(at(10));
    harness.runtime.read_archived_positions().await.unwrap();
    assert!(held(&harness).is_some());

    harness
        .exchange
        .fail_positions(10, "info endpoint unavailable");
    harness.clock.set(at(70));
    let _ = harness.runtime.read_archived_positions().await;
    let stale = held(&harness).unwrap();
    assert_eq!(stale.read_at, at(10));

    harness.clock.set(at(10 + 301));
    let _ = harness.runtime.read_archived_positions().await;
    assert_eq!(held(&harness), None);
}

/// B62：不足最低下单金额的零头也算「仍有仓位」：把整个仓位平掉的只减仓单不受最低金额
/// 限制（附录 A），可以市价平仓，平到 0 为止。
#[tokio::test]
async fn b62_a_dust_position_is_listed_and_closed_to_zero() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    // 0.001 × 5000 = 5 美元，不足 10 美元。
    harness
        .exchange
        .set_position(Position::parse("0.001").unwrap());
    harness.exchange.set_market_price(price("5000"), at(9));

    harness.clock.set(at(10));
    harness.runtime.read_archived_positions().await.unwrap();
    assert_eq!(
        held(&harness).unwrap().position,
        Position::parse("0.001").unwrap()
    );

    assert_eq!(
        harness.runtime.close(&harness.strategy_id).await.unwrap(),
        StrategyStatus::Closing
    );
    harness.step(11).await;
    let closes = close_orders(&harness);
    assert_eq!(closes.len(), 1);
    assert_eq!(closes[0].record.quantity, quantity("0.001"));
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    assert_eq!(harness.step(12).await.status, StrategyStatus::Closed);
}

/// 归档之后在交易所手动改过仓位，再平仓时平仓单结果未知：期限过后查无此单、没有成交，
/// 要能作废它、按实际仓位重新平，不能因为账上的应有持仓对不上就一直卡在平仓中。
#[tokio::test]
async fn an_unknown_close_after_a_manual_trade_is_voided_and_retried() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    // 归档之后在交易所手动平掉一部分：账上的应有持仓和实际仓位不再一致。
    let half = Position::parse("0.02").unwrap();
    assert!(harness.exchange.current_position() > half);
    harness.exchange.set_position(half);

    harness.clock.set(at(10));
    harness.runtime.close(&harness.strategy_id).await.unwrap();
    harness.exchange.unknown_next_place("timeout");
    harness.step(11).await;
    assert_eq!(
        close_orders(&harness)[0].record.status,
        OrderStatus::Unknown
    );
    assert_eq!(harness.exchange.current_position(), half);

    // 期限（下单时刻 + 5 秒）和核对余量（30 秒）都过去之后：作废它，按实际仓位重新平。
    for second in [60, 61, 62, 63] {
        if harness.step(second).await.status == StrategyStatus::Closed {
            break;
        }
    }
    let closes = close_orders(&harness);
    assert_eq!(closes[0].record.status, OrderStatus::Void);
    assert_eq!(closes.len(), 2);

    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    assert_eq!(status(&harness), StrategyStatus::Closed);
}

/// 已确认平仓、还在停止中收尾时再停一次（命令行紧急停止就是这样）：保留平仓，收尾之后
/// 照样进入平仓中。已经在平仓中时再停才是放弃平仓（B56）。
#[tokio::test]
async fn stopping_again_keeps_a_confirmed_close() {
    let harness = holding().await;
    harness.exchange.unknown_next_cancels(2, "timeout");
    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();
    assert_eq!(status(&harness), StrategyStatus::Stopping);

    harness.clock.set(at(3));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    for second in 4..10 {
        if status(&harness) != StrategyStatus::Stopping {
            break;
        }
        harness.step(second).await;
    }

    // 收尾之后进入平仓中，而不是保留仓位归档。
    assert_eq!(status(&harness), StrategyStatus::Closing);
}

/// 已归档的策略「停止并平仓」：停止已经做过了，平仓照样做，不静默忽略。
#[tokio::test]
async fn stop_and_close_on_an_archived_strategy_closes() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    assert_eq!(status(&harness), StrategyStatus::Stopped);

    harness.clock.set(at(3));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();

    assert_eq!(status(&harness), StrategyStatus::Closing);
}

/// 仓位读数还在变的时候不发平仓单：按还没追上的仓位算出来的数量是错的。读数稳定之后照常
/// 平掉。
#[tokio::test]
async fn a_close_waits_for_a_stable_reading() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    harness
        .exchange
        .set_position(Position::parse("0.02").unwrap());
    harness.clock.set(at(10));
    harness.runtime.close(&harness.strategy_id).await.unwrap();

    // 这一步的四次仓位查询读数各不相同：核对结论是「还在变」。
    for value in ["0.03", "0.04", "0.03", "0.04"] {
        harness
            .exchange
            .report_stale_positions(1, Position::parse(value).unwrap());
    }
    harness.step(11).await;
    assert!(close_orders(&harness).is_empty());

    harness.step(12).await;
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    assert_eq!(harness.step(13).await.status, StrategyStatus::Closed);
}

/// 归档后手动改过仓位、重新平仓时平仓单结果未知，这时停止（放弃平仓）：停止中的收尾同样
/// 作废这张单（平仓单作废不看仓位一致），能归档，不会卡在停止中。
#[tokio::test]
async fn giving_up_a_close_settles_an_unknown_close_order() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    harness
        .exchange
        .set_position(Position::parse("0.02").unwrap());
    harness.clock.set(at(10));
    harness.runtime.close(&harness.strategy_id).await.unwrap();
    harness.exchange.unknown_next_place("timeout");
    harness.step(11).await;
    assert_eq!(
        close_orders(&harness)[0].record.status,
        OrderStatus::Unknown
    );

    harness.clock.set(at(12));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    assert_eq!(status(&harness), StrategyStatus::Stopping);
    for second in [60, 61, 62] {
        if status(&harness) != StrategyStatus::Stopping {
            break;
        }
        harness.step(second).await;
    }

    assert_eq!(status(&harness), StrategyStatus::Stopped);
    assert_eq!(close_orders(&harness)[0].record.status, OrderStatus::Void);
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.02").unwrap()
    );
}

/// 仓位读数还在变、最后一次恰好读到 0：不能当成已经平完归档，真实仓位还在。
#[tokio::test]
async fn an_unstable_zero_reading_does_not_finish_the_close() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness.runtime.stop(&harness.strategy_id).await.unwrap();
    harness
        .exchange
        .set_position(Position::parse("0.02").unwrap());
    harness.clock.set(at(10));
    harness.runtime.close(&harness.strategy_id).await.unwrap();

    for value in ["0.03", "0.04", "0.03", "0"] {
        harness
            .exchange
            .report_stale_positions(1, Position::parse(value).unwrap());
    }
    harness.step(11).await;
    assert_eq!(status(&harness), StrategyStatus::Closing);

    harness.step(12).await;
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    assert_eq!(harness.step(13).await.status, StrategyStatus::Closed);
}

/// 平仓途中在交易所手动交易过：账上的应有持仓和实际对不上，结果未知的平仓单照样能作废、
/// 按实际仓位重新平。
#[tokio::test]
async fn a_manual_trade_during_the_close_does_not_block_an_unknown_close() {
    let harness = holding().await;
    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();
    // 第一张平仓单只成交一部分。
    harness.exchange.cap_next_ioc_fill(quantity("0.01"));
    harness.step(3).await;
    harness.step(4).await;
    // 平仓途中手动平掉一部分，下一张平仓单结果未知。
    harness
        .exchange
        .set_position(Position::parse("0.01").unwrap());
    harness.exchange.unknown_next_place("timeout");
    harness.step(5).await;
    harness.step(6).await;
    let unknown = close_orders(&harness)
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Unknown)
        .expect("an unknown close order");

    for second in [60, 61, 62, 63] {
        if harness.step(second).await.status == StrategyStatus::Closed {
            break;
        }
    }

    let voided = close_orders(&harness)
        .into_iter()
        .find(|order| order.record.client_order_id == unknown.record.client_order_id)
        .unwrap();
    assert_eq!(voided.record.status, OrderStatus::Void);
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    assert_eq!(status(&harness), StrategyStatus::Closed);
}

/// 平仓成交已经记账、仓位接口还停在旧值上（连着几次都一样）：按旧仓位多发的只减仓单
/// 不会平过头，仓位追上之后照常平完。
#[tokio::test]
async fn a_lagging_position_after_a_close_fill_still_closes() {
    let harness = holding().await;
    let held = harness.exchange.current_position();
    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();
    harness.exchange.cap_next_ioc_fill(quantity("0.01"));
    harness.step(3).await;
    assert_eq!(close_orders(&harness).len(), 1);

    // 这一步的仓位查询都还停在平仓之前。
    harness.exchange.report_stale_positions(4, held);
    harness.step(4).await;
    // 只减仓：按旧仓位多发的数量成交不了，仓位不会被翻到空头。
    assert!(harness.exchange.current_position() >= Position::ZERO);

    for second in 5..10 {
        if harness.step(second).await.status == StrategyStatus::Closed {
            break;
        }
    }
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    assert_eq!(status(&harness), StrategyStatus::Closed);
}

/// 平仓平掉的是整个净仓位，含账本外的仓位（用户手动开的）：那一部分不是这个策略的，不算进
/// 减仓已实现，归档之后再平掉的也一样。账本上的部分按平仓成交价减去账本成本。
#[tokio::test]
async fn a_close_counts_only_the_ledger_part_as_realized() {
    let harness = core_harness().await;
    // 手续费记为 0：这里只看归属，手续费的算法在策略的单元测试里。
    harness.exchange.set_fee_rate(Rate::ZERO);
    harness.step(0).await;
    harness.exchange.set_market_price(price("89000"), at(1));
    harness.step(1).await;
    let before = harness.runtime.detail(&harness.strategy_id).unwrap();
    let held = before
        .batches
        .iter()
        .map(|batch| (batch.core_filled, batch.price))
        .chain(
            before
                .cells
                .iter()
                .map(|cell| (cell.held, cell.entry_price)),
        );
    let (ledger, cost) = held.fold(
        (Quantity::ZERO, Notional::ZERO),
        |(ledger, cost), (quantity, price)| {
            (ledger + quantity, cost + Notional::of(quantity, price))
        },
    );
    assert_eq!(
        Position::from_quantity(ledger),
        harness.exchange.current_position()
    );

    harness.clock.set(at(2));
    harness
        .runtime
        .stop_and_close(&harness.strategy_id)
        .await
        .unwrap();
    harness.exchange.manual_trade(quantity("0.1"));
    for second in 3..10 {
        if harness.step(second).await.status == StrategyStatus::Closed {
            break;
        }
    }
    assert_eq!(status(&harness), StrategyStatus::Closed);

    // 平仓单按标记价让出 5% 成交：89000 × .95 = 84550。
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    let realized = Notional::of(ledger, price("84550")) - cost;
    assert_eq!(detail.reduction_realized_profit, Some(realized));
    assert_eq!(detail.average_price, None);
    assert_eq!(detail.unrealized_pnl, Notional::ZERO);
    assert_eq!(detail.total_pnl, detail.grid_realized_profit + realized);

    // 归档之后又平掉一笔账本外的仓位（价格不同）：不改写这个策略已有的盈亏。
    harness.exchange.manual_trade(quantity("0.1"));
    harness.exchange.set_market_price(price("95000"), at(20));
    harness.clock.set(at(20));
    harness.runtime.close(&harness.strategy_id).await.unwrap();
    for second in 21..30 {
        if harness.step(second).await.status == StrategyStatus::Closed {
            break;
        }
    }
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    let detail = harness.runtime.detail(&harness.strategy_id).unwrap();
    assert_eq!(detail.reduction_realized_profit, Some(realized));
}
