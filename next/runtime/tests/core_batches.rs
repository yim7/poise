//! 主仓批次与格子跟随在模拟交易所上的完整过程。
//!
//! 配置是设计第 3.2 节的例子加上主仓：`[75000, 95000]`，`M = 1`，`r = .7`，`n = 4`，
//! `d = g = 1000`。四批的主仓价是 95000、90000、85000、80000，每批 `C = .175`、
//! `G = .075`，跟随每批的 5 格各 `.015`。

mod common;

use common::{
    at, btc_fees, btc_spec, core_harness, core_long_config, core_only_harness, long_batch,
    long_cell, long_state,
};
use poise_strategy::batch::BatchIndex;
use poise_strategy::order::{OrderPurpose, OrderStatus, Unit};
use poise_strategy::units::{Position, Price, Quantity};

/// 第 `cell` 格的买价：95000 往下每格 1000。
fn cell_buy_price(cell: usize) -> Price {
    Price::parse(&(95000 - cell * 1000).to_string()).unwrap()
}

/// 第 `batch` 批的主仓价：95000 往下每批 5000。
fn batch_price(batch: usize) -> Price {
    Price::parse(&(95000 - batch * 5000).to_string()).unwrap()
}

#[tokio::test]
async fn b01_a_downturn_and_a_recovery_build_the_core_and_cycle_the_cells() {
    let harness = core_harness();
    // 从 96 开始：四批主仓买单挂在各自批次价，都不成交；格子全部未启用。
    harness
        .exchange
        .set_market_price(Price::parse("96000").unwrap(), at(0));
    let start = harness.step(0).await;

    assert_eq!(start.placed.len(), 4);
    assert_eq!(start.position, Position::ZERO);
    for batch in 0..4 {
        let order = harness
            .at_exchange(&harness.working_core_cloid(batch))
            .unwrap();
        assert_eq!(order.limit_price, batch_price(batch));
        assert_eq!(order.quantity, Quantity::parse("0.175").unwrap());
        assert_eq!(harness.core_filled(batch), Quantity::ZERO);
    }
    assert!((0..20).all(|cell| !harness.cell(cell).enabled));

    // 跌到 88：95、90 两批主仓成交，跟随它们的格子启用并按各自的买价买入。
    harness
        .exchange
        .set_market_price(Price::parse("88000").unwrap(), at(1));
    let fall = harness.step(1).await;

    assert_eq!(harness.core_filled(0), Quantity::parse("0.175").unwrap());
    assert_eq!(harness.core_filled(1), Quantity::parse("0.175").unwrap());
    assert_eq!(harness.core_filled(2), Quantity::ZERO);
    // 两批的 10 格全部启用，其中 95 到 88 共 8 格买价不低于当前价、当场买入。
    assert_eq!(fall.placed.len(), 10);
    // 仓位核对发生在下单之前：那一刻交易所只有主仓 .35。
    assert_eq!(fall.position, fall.expected_position);
    assert_eq!(fall.position, Position::parse("0.35").unwrap());
    // 8 格买完，交易所上的仓位是主仓 .35 加 8 格各 .015。
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.47").unwrap()
    );
    for cell in 0..8 {
        assert!(harness.cell(cell).enabled);
        assert_eq!(harness.cell(cell).to_buy_back, Quantity::ZERO);
    }
    // 87、86 两格已启用但还没轮到，挂着各自的买单等价格下来。
    for cell in 8..10 {
        assert!(harness.cell(cell).enabled);
        let buy = harness
            .at_exchange(&harness.working_cloid(cell, OrderPurpose::GridEntry))
            .unwrap();
        assert_eq!(buy.limit_price, cell_buy_price(cell));
        assert_eq!(
            harness.cell(cell).to_buy_back,
            Quantity::parse("0.015").unwrap()
        );
    }
    // 85、80 两批的主仓买单还在等。
    assert_eq!(harness.core_filled(3), Quantity::ZERO);

    // 价格还在 88：买过的 8 格按买价加一个价差 1000 挂出卖单。
    let sells = harness.step(2).await;

    assert_eq!(sells.placed.len(), 8);
    assert_eq!(sells.position, Position::parse("0.47").unwrap());
    assert_eq!(sells.expected_position, Position::parse("0.47").unwrap());
    for cell in 0..8 {
        let sell = harness
            .at_exchange(&harness.working_cloid(cell, OrderPurpose::GridExit))
            .unwrap();
        assert_eq!(
            sell.limit_price,
            Price::parse(&(96000 - cell * 1000).to_string()).unwrap()
        );
        assert_eq!(sell.quantity, Quantity::parse("0.015").unwrap());
        assert!(sell.reduce_only);
    }

    // 回升到 96：买过的 8 格全部按各自的卖价卖出，并重新挂回自己的买价。
    harness
        .exchange
        .set_market_price(Price::parse("96000").unwrap(), at(3));
    let recovery = harness.step(3).await;

    // 卖单在价格回到 96 时就成交了，核对时交易所上只剩主仓。
    assert_eq!(recovery.position, Position::parse("0.35").unwrap());
    assert_eq!(recovery.expected_position, Position::parse("0.35").unwrap());
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("0.35").unwrap()
    );
    for cell in 0..8 {
        assert_eq!(
            harness.cell(cell).to_buy_back,
            Quantity::parse("0.015").unwrap()
        );
        let buy = harness
            .at_exchange(&harness.working_cloid(cell, OrderPurpose::GridEntry))
            .unwrap();
        assert_eq!(buy.limit_price, cell_buy_price(cell));
    }
    assert!(
        harness
            .orders()
            .iter()
            .filter(|order| order.record.purpose == OrderPurpose::GridExit)
            .all(|order| order.record.status == OrderStatus::Filled),
        "every cell that bought was sold"
    );
    // 只剩主仓 .35，加上还没成交的 2 格买回和 2 批主仓买单。
    assert_eq!(harness.working().len(), 12);
    // 主仓只买不卖：全过程没有一张卖主仓的订单。
    assert!(
        harness
            .orders()
            .iter()
            .filter(|order| matches!(order.record.unit, Unit::Batch(..)))
            .all(|order| order.record.purpose == OrderPurpose::Core),
        "the core is never sold"
    );

    // 再走一步不会重复挂单。
    let idle = harness.step(4).await;
    assert!(idle.placed.is_empty());
    assert!(idle.canceled.is_empty());
    assert_eq!(idle.position, Position::parse("0.35").unwrap());
}

#[tokio::test]
async fn b28_a_full_core_ratio_buys_only_the_core() {
    let harness = core_only_harness();
    harness
        .exchange
        .set_market_price(Price::parse("96000").unwrap(), at(0));

    // 启动预览同样没有网格：只有四批主仓，没有一格。
    let preview = harness
        .runtime
        .preview_startup(common::core_only_config(), btc_spec(), btc_fees())
        .await
        .unwrap();
    assert_eq!(preview.batches.len(), 4);
    assert!(
        preview
            .batches
            .iter()
            .all(|batch| batch.cell_count == 0 && batch.grid_quantity == Quantity::ZERO)
    );
    assert!(preview.immediate_fills.is_empty());

    let start = harness.step(0).await;

    // 四批主仓买单，没有格子，所以没有任何网格订单。
    assert_eq!(start.placed.len(), 4);
    assert_eq!(harness.working().len(), 4);
    assert_eq!(start.position, Position::ZERO);

    // 一路跌到 78：四批主仓全部成交，持仓正好是四批主仓之和 1。
    let mut last = start;
    for (second, price) in [(1, "94000"), (2, "89000"), (3, "84000"), (4, "78000")] {
        harness
            .exchange
            .set_market_price(Price::parse(price).unwrap(), at(second));
        last = harness.step(second).await;
    }

    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("1").unwrap()
    );
    let state = harness.runtime.state(&harness.strategy_id).unwrap();
    assert_eq!(
        long_state(&state).core_position(),
        Quantity::parse("1").unwrap(),
        "the whole position is core"
    );
    // 应有持仓等于已买入的主仓：没有格子，也没有待买回量。
    assert_eq!(last.position, Position::parse("1").unwrap());
    assert_eq!(last.expected_position, Position::parse("1").unwrap());
    assert!(
        long_state(&state).cells().is_empty(),
        "r = 1 has no cells at all"
    );
    assert!(
        harness
            .orders()
            .iter()
            .all(|order| order.record.purpose == OrderPurpose::Core),
        "r = 1 never sells"
    );

    // 价格回升也不会卖出：主仓只买不卖。
    harness
        .exchange
        .set_market_price(Price::parse("96000").unwrap(), at(5));
    let recovery = harness.step(5).await;

    assert!(recovery.placed.is_empty());
    assert_eq!(recovery.position, Position::parse("1").unwrap());
    assert_eq!(recovery.expected_position, Position::parse("1").unwrap());
}

#[tokio::test]
async fn the_startup_preview_shows_the_batch_table_and_the_immediate_fills() {
    let harness = core_harness();
    harness
        .exchange
        .set_market_price(Price::parse("88000").unwrap(), at(0));

    let preview = harness
        .runtime
        .preview_startup(core_long_config(), btc_spec(), btc_fees())
        .await
        .unwrap();

    assert_eq!(preview.market_price, Price::parse("88000").unwrap());
    assert_eq!(preview.batches.len(), 4);
    for (index, batch) in preview.batches.iter().enumerate() {
        assert_eq!(batch.index, BatchIndex(index));
        assert_eq!(batch.price, batch_price(index));
        assert_eq!(batch.core_quantity, Quantity::parse("0.175").unwrap());
        assert_eq!(batch.grid_quantity, Quantity::parse("0.075").unwrap());
        assert_eq!(batch.cell_count, 5);
        assert_eq!(batch.enabled_cell_count, 0);
    }
    // 95、90 两批的主仓立即买入，跟随它们的 95 到 88 共 8 格随即买入。
    assert_eq!(preview.immediate_fills.len(), 10);
    assert_eq!(preview.immediate_fills[0].unit, long_batch(0));
    assert_eq!(preview.immediate_fills[0].purpose, OrderPurpose::Core);
    assert_eq!(
        preview.immediate_fills[0].quantity,
        Quantity::parse("0.175").unwrap()
    );
    let last = preview.immediate_fills.last().unwrap();
    assert_eq!(last.unit, long_cell(7));
    assert_eq!(last.purpose, OrderPurpose::GridEntry);
    assert_eq!(last.limit_price, Price::parse("88000").unwrap());
    assert_eq!(last.quantity, Quantity::parse("0.015").unwrap());
}
