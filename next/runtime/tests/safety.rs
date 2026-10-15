//! 安全底线：先写库再发送、结果未知时保留预留、仓位核对失败就暂停。

mod common;

use common::{at, btc, btc_fees, btc_spec, harness, long_grid_config};
use poise_runtime::{
    ResumeOutcome, Runtime, RuntimeError, SimulatedExchange, Store, StoredOrder, StrategyStatus,
};
use poise_strategy::grid::CellIndex;
use poise_strategy::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus};
use poise_strategy::planning::WaitReason;
use poise_strategy::units::{Position, Price, Quantity};

#[tokio::test]
async fn a_rejected_order_is_written_to_the_database_before_it_is_sent() {
    let harness = harness();
    harness.exchange.reject_next_place("insufficient margin");

    let report = harness.step(0).await;

    // 20 张单都尝试发送；被拒绝的那张仍然留在数据库里，状态是已拒绝。
    assert_eq!(report.placed.len(), 20);
    let rejected = harness
        .orders()
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Rejected)
        .expect("the rejected order must stay in the database");
    assert!(
        harness
            .at_exchange(&rejected.record.client_order_id)
            .is_none()
    );
    assert_eq!(harness.working().len(), 19);

    // 下一步按状态重新挂上，换新的客户端订单 ID。
    let report = harness.step(1).await;

    assert_eq!(report.placed.len(), 1);
    assert_ne!(report.placed[0], rejected.record.client_order_id);
    assert_eq!(harness.working().len(), 20);
}

#[tokio::test]
async fn an_order_the_exchange_does_not_know_keeps_its_reservation() {
    let harness = harness();
    harness.exchange.unknown_next_place("timed out");

    let report = harness.step(0).await;

    assert_eq!(report.placed.len(), 20);
    let unknown = harness
        .orders()
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Unknown)
        .expect("the unknown order must stay in the database");

    // 还没过 expiresAfter：保留它的预留，不重发。
    let report = harness.step(1).await;

    assert!(report.placed.is_empty());
    assert_eq!(harness.working().len(), 20);

    // 过了 expiresAfter 也一样：交易所查不到这张单，凭过期就断定它没有影响是不安全的，
    // 所以预留照旧保留、不重发，只在规划里说明原因（第 9.2 节）。
    let report = harness.step(6).await;

    assert!(report.placed.is_empty());
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    assert_eq!(harness.working().len(), 20);
    assert_eq!(
        harness
            .orders()
            .into_iter()
            .find(|order| order.record.client_order_id == unknown.record.client_order_id)
            .unwrap()
            .record
            .status,
        OrderStatus::Unknown
    );
}

#[tokio::test]
async fn a_cancel_with_an_unknown_result_keeps_the_reservation_and_places_nothing() {
    let harness = harness();
    // 数据库里留着一张交易所并不认识的挂单（上一次进程崩溃时的记录）：
    // 撤单只会得到「查无此单」，这不能当成撤销成功。
    let ghost = ClientOrderId::try_new("0xghost").unwrap();
    harness
        .runtime
        .store()
        .insert_order(
            &harness.strategy_id,
            &StoredOrder {
                record: OrderRecord {
                    client_order_id: ghost.clone(),
                    cell_index: CellIndex(0),
                    purpose: OrderPurpose::GridBuy,
                    // 价格与格子买价不符，规划会要求撤掉它、再补一张新的。
                    limit_price: Price::parse("94000").unwrap(),
                    quantity: Quantity::parse("0.05").unwrap(),
                    cumulative_filled: Quantity::ZERO,
                    reduce_only: false,
                    status: OrderStatus::Open,
                },
                exchange_order_id: None,
                expires_at: at(6),
                placed_at: at(0),
                updated_at: at(0),
            },
        )
        .unwrap();
    let place_count = harness.exchange.place_count();

    let report = harness.step(1).await;

    // 规划想撤掉这张单、再补上 20 格的买单；撤单结果未知时这一步什么也不做。
    assert_eq!(report.plan.cancel, vec![ghost.clone()]);
    assert_eq!(report.plan.place.len(), 20);
    assert!(report.canceled.is_empty());
    assert!(report.placed.is_empty());
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    assert_eq!(harness.exchange.place_count(), place_count);
    // 预留还在：这张单仍然被当作在场，不会重发。
    assert!(
        harness
            .working()
            .iter()
            .any(|order| order.record.client_order_id == ghost)
    );
}

#[tokio::test]
async fn a_position_mismatch_explained_by_an_unrecorded_fill_does_not_pause() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridBuy);

    // 交易所已经成交 0.02，但这笔成交的时间戳早于拉取窗口，我们还没记入。
    // 挂单快照里带着这个累计量，仓位差额能由它解释（第 7.1.3 节）。
    harness
        .exchange
        .inject_fill(
            "trade-ancient",
            &buy,
            Quantity::parse("0.02").unwrap(),
            Quantity::parse("0.02").unwrap(),
            at(-600),
        )
        .unwrap();

    let report = harness.step(1).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 0);
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    // 状态没有变：这一步照常挂单，不会因为差额而下单或撤单。
    assert!(report.placed.is_empty());
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.05").unwrap()
    );
}

#[tokio::test]
async fn a_cancel_that_fails_while_pausing_is_retried_by_pausing_again() {
    let harness = harness();
    harness.step(0).await;
    harness
        .exchange
        .manual_trade(Quantity::parse("0.01").unwrap());
    // 第一次撤单请求超时：这张单还留在交易所，策略仍然暂停。
    harness.exchange.unknown_next_cancel("timed out");

    let report = harness.step(1).await;

    assert!(report.is_paused());
    assert_eq!(report.canceled.len(), 19);
    assert_eq!(harness.working().len(), 1);

    // 再暂停一次就能把剩下的撤掉：`pause` 可以重复调用。
    let canceled = harness
        .runtime
        .pause(&harness.strategy_id, at(2))
        .await
        .unwrap();

    assert_eq!(canceled.len(), 1);
    assert!(harness.working().is_empty());
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working())
    );
}

#[tokio::test]
async fn pausing_absorbs_a_late_fill_older_than_the_fills_lookback() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridBuy);
    // 下单当时就成交了 0.02，但成交记录迟迟不到。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.02").unwrap(), at(0))
        .unwrap();
    // 又走了很久：同步点被推到 at(120)，拉取窗口的下限因此高过这笔成交的时间。
    harness.step(120).await;
    harness.exchange.deliver_delayed_fills();

    let canceled = harness
        .runtime
        .pause(&harness.strategy_id, at(121))
        .await
        .unwrap();

    // 暂停要吸收最终成交（第 9.4 节）：这笔成交比同步点早得多，但订单是那时挂出去的，
    // 所以它仍然在拉取范围内，不能漏掉。
    assert_eq!(canceled.len(), 20);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
}

#[tokio::test]
async fn a_fully_filled_order_whose_fill_record_is_late_is_not_replaced() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridBuy);
    // 这张买单在交易所全部成交了，但成交记录要下一步才查得到。
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&buy, Quantity::parse("0.05").unwrap(), at(1))
        .unwrap();
    let place_count = harness.exchange.place_count();

    let report = harness.step(1).await;

    // 订单从挂单列表里消失，但交易所说它已成交：不能判成撤销、不能在同一个格子
    // 再挂一张新买单，也不能因为仓位多了一份就暂停。
    assert!(!report.is_paused());
    assert!(report.placed.is_empty());
    assert_eq!(harness.exchange.place_count(), place_count);
    assert_eq!(report.position, Position::parse("0.05").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.05").unwrap());
    assert_eq!(harness.orders().len(), 20);
    assert_eq!(harness.working().len(), 20);

    // 成交记录到了：补记进格子，订单收尾，然后照常挂出这一格的卖单。
    harness.exchange.deliver_delayed_fills();
    let report = harness.step(2).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 1);
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::ZERO,
        "the whole cell was bought"
    );
    assert_eq!(harness.working().len(), 20);
    let sell = harness.working_cloid(0, OrderPurpose::GridSell);
    assert_eq!(
        harness.at_exchange(&sell).unwrap().limit_price,
        Price::parse("96000").unwrap()
    );
}

#[tokio::test]
async fn a_fill_between_the_fills_query_and_the_position_query_does_not_pause() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridBuy);
    // 查完成交、还没查仓位时成交了 0.02。
    harness
        .exchange
        .fill_before_next_position(buy, Quantity::parse("0.02").unwrap());

    let report = harness.step(1).await;

    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 1);
    assert_eq!(report.position, Position::parse("0.02").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.02").unwrap());
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap()
    );
}

#[tokio::test]
async fn a_fill_between_the_position_query_and_the_fills_requery_does_not_pause() {
    let harness = harness();
    harness.step(0).await;
    let buy = harness.working_cloid(0, OrderPurpose::GridBuy);
    // 查成交、查仓位之间成交 0.02：仓位快照里没有它，于是触发补拉成交；
    // 补拉成交之前又成交了 0.02，这一笔同样不在刚才那份仓位快照里。
    harness
        .exchange
        .fill_before_next_position(buy.clone(), Quantity::parse("0.02").unwrap());
    harness
        .exchange
        .fill_after_next_position(buy, Quantity::parse("0.02").unwrap());

    let report = harness.step(1).await;

    // 补拉之后重新查一次仓位：两笔都记入，仓位和预期一致，不是真的对不上。
    assert!(!report.is_paused());
    assert_eq!(report.applied_fills, 2);
    assert_eq!(report.position, Position::parse("0.04").unwrap());
    assert_eq!(report.expected_position, Position::parse("0.04").unwrap());
    // 这一步照常往下走：买到的 0.04 挂出卖单，原来的买单还剩 0.01 在场。
    assert_eq!(report.placed.len(), 1);
    assert_eq!(
        harness.at_exchange(&report.placed[0]).unwrap().limit_price,
        Price::parse("96000").unwrap()
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.01").unwrap()
    );
}

#[tokio::test]
async fn a_cancel_of_an_order_the_exchange_never_saw_keeps_the_reservation() {
    let harness = harness();
    // 下单超时：交易所根本没有这张单。
    harness.exchange.unknown_next_place("timed out");
    harness.step(0).await;
    let unknown = harness
        .orders()
        .into_iter()
        .find(|order| order.record.status == OrderStatus::Unknown)
        .expect("the unknown order must stay in the database");
    let place_count = harness.exchange.place_count();

    // 过了 expiresAfter 撤它，交易所只会回答「查无此单」：这分不出它是已成交、
    // 已撤销还是压根没落地，所以和挂单列表里消失时一样保留预留、不释放。
    let canceled = harness
        .runtime
        .pause(&harness.strategy_id, at(6))
        .await
        .unwrap();

    assert_eq!(canceled.len(), 19);
    assert_eq!(harness.exchange.place_count(), place_count);
    assert_eq!(
        harness
            .orders()
            .into_iter()
            .find(|order| order.record.client_order_id == unknown.record.client_order_id)
            .unwrap()
            .record
            .status,
        OrderStatus::Unknown
    );

    // 预留还在：恢复后只为被撤掉的那 19 格补单，这一格不会补挂新的买单。
    let outcome = harness
        .runtime
        .resume(&harness.strategy_id, at(7))
        .await
        .unwrap();
    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("expected a step, got {outcome:?}");
    };
    assert_eq!(report.placed.len(), 19);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::OrderResultUnknown)
    );
    let buys: Vec<_> = harness
        .orders()
        .into_iter()
        .filter(|order| {
            order.record.cell_index == unknown.record.cell_index
                && order.record.purpose == OrderPurpose::GridBuy
        })
        .collect();
    assert_eq!(buys.len(), 1);
    assert_eq!(
        buys[0].record.client_order_id,
        unknown.record.client_order_id
    );
}

/// 部分成交之后被交易所自己撤掉（自成交保护、只减仓冲突这类终态）：
/// 成交照记进格子，剩下的预留释放出来重新挂。
#[tokio::test]
async fn a_partially_filled_order_canceled_by_the_exchange_is_booked_and_released() {
    let harness = harness();
    harness.step(0).await;

    // 第 0 格的买单成交 0.02（成交记录暂时查不到），随后交易所把它撤掉。
    let cloid = harness.working_cloid(0, OrderPurpose::GridBuy);
    harness.exchange.delay_next_fills(1);
    harness
        .exchange
        .fill(&cloid, Quantity::parse("0.02").unwrap(), at(1))
        .unwrap();
    assert!(harness.exchange.cancel_externally(&cloid));

    let report = harness.step(1).await;

    // 撤销带回了最终的累计成交量：0.02 记进格子，这张单是终态。
    let order = harness
        .orders()
        .into_iter()
        .find(|order| order.record.client_order_id == cloid)
        .expect("the canceled order must stay in the database");
    assert_eq!(order.record.status, OrderStatus::Canceled);
    assert_eq!(
        order.record.cumulative_filled,
        Quantity::parse("0.02").unwrap()
    );
    assert_eq!(
        harness.cell(0).to_buy_back,
        Quantity::parse("0.03").unwrap(),
        "买到的 0.02 不再算待买回"
    );

    // 剩余预留释放：这一格补挂 0.03 的买单，买到的 0.02 挂出卖单。
    assert_eq!(report.placed.len(), 2);
    let placed: Vec<_> = harness
        .working()
        .into_iter()
        .filter(|order| order.record.cell_index == CellIndex(0))
        .collect();
    let buy = placed
        .iter()
        .find(|order| order.record.purpose == OrderPurpose::GridBuy)
        .expect("the remaining quantity must be placed again");
    assert_eq!(buy.record.quantity, Quantity::parse("0.03").unwrap());
    let sell = placed
        .iter()
        .find(|order| order.record.purpose == OrderPurpose::GridSell)
        .expect("the filled quantity must be offered for sale");
    assert_eq!(sell.record.quantity, Quantity::parse("0.02").unwrap());
}

#[tokio::test]
async fn b18_a_position_mismatch_pauses_the_strategy_and_cancels_its_orders() {
    let harness = harness();
    harness.step(0).await;
    // 用户在交易所手动买入 0.01。
    harness
        .exchange
        .manual_trade(Quantity::parse("0.01").unwrap());

    let report = harness.step(1).await;

    assert!(report.is_paused());
    assert_eq!(report.position, Position::parse("0.01").unwrap());
    assert_eq!(report.expected_position, Position::ZERO);
    assert!(
        report
            .plan
            .waiting
            .contains(&WaitReason::PositionCheckFailed)
    );
    assert!(harness.working().is_empty());
    assert!(
        harness
            .exchange
            .orders()
            .iter()
            .all(|order| !order.status.is_working())
    );
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );

    // 暂停后不再走步。
    assert!(matches!(
        harness.runtime.step(&harness.strategy_id, at(2)).await,
        Err(RuntimeError::NotRunning(_, StrategyStatus::Paused))
    ));

    // 仓位没修好之前，恢复会立刻再次暂停。
    let outcome = harness
        .runtime
        .resume(&harness.strategy_id, at(3))
        .await
        .unwrap();

    let ResumeOutcome::Resumed(report) = outcome else {
        panic!("expected a step, got {outcome:?}");
    };
    assert!(report.is_paused());
    assert_eq!(
        harness.runtime.status(&harness.strategy_id).unwrap(),
        StrategyStatus::Paused
    );
}

#[tokio::test]
async fn b23_an_invalid_config_is_refused_before_anything_is_stored() {
    let harness = harness();
    let mut config = long_grid_config();
    // 价差 1 美元覆盖不了两条腿的 taker 费率。
    config.long.grid_spread = poise_strategy::units::Price::parse("1").unwrap();

    let error = harness
        .runtime
        .create_strategy(config, btc_spec(), btc_fees(), at(0))
        .unwrap_err();

    assert!(matches!(error, RuntimeError::Config(_)));
    // 数据库仍然可用：同一批参数换一个合法的价差就能创建。
    let mut valid = long_grid_config();
    valid.long.grid_spread = poise_strategy::units::Price::parse("1000").unwrap();
    let strategy_id = harness
        .runtime
        .create_strategy(valid, btc_spec(), btc_fees(), at(0))
        .unwrap();
    assert_eq!(
        harness.runtime.status(&strategy_id).unwrap(),
        StrategyStatus::Running
    );
}

#[tokio::test]
async fn step_on_an_unknown_strategy_is_refused() {
    let harness = harness();
    let missing = poise_runtime::StrategyId::try_from("s-missing".to_string()).unwrap();

    let error = harness.runtime.step(&missing, at(0)).await.unwrap_err();

    assert!(matches!(
        error,
        RuntimeError::Store(poise_runtime::StoreError::UnknownStrategy(_))
    ));
}

#[test]
fn the_runtime_can_be_built_from_any_exchange_implementation() {
    // 只验证 trait 的形状：模拟交易所可以作为 `Arc<dyn Exchange>` 传进去。
    let exchange: std::sync::Arc<dyn poise_runtime::Exchange> = common::exchange();
    let runtime = Runtime::new(exchange, Store::open_in_memory().unwrap());

    assert_eq!(runtime.options().order_expires_after.num_seconds(), 5);
    assert_eq!(
        SimulatedExchange::new(
            btc(),
            poise_strategy::units::Price::parse("1").unwrap(),
            at(0)
        )
        .current_position(),
        Position::ZERO
    );
}
