//! 做空网格与中性网格在模拟交易所上的完整过程（设计第 5、13 节 B10、B11）。
//!
//! 做空侧是多头侧的镜像：开空价从 `L` 往上，格子先卖开空、再按开空价减一个价差买平，
//! 向下突破只留主仓空单。中性网格两侧都没有主仓，净仓位在多空之间穿越零，格子单不标
//! 只减仓。

mod common;

use chrono::Duration;
use common::{
    at, btc, core_short_harness, harness_with_spec, neutral_grid_config, neutral_harness,
    short_cell,
};
use poise_runtime::Store;
use poise_strategy::config::{SideConfig, StrategyConfig};
use poise_strategy::instrument::{InstrumentSpec, PriceRule};
use poise_strategy::order::{OrderPurpose, Side, Unit};
use poise_strategy::side::Direction;
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};

/// 第 `cell` 格的开空价：75000 往上每格 1000。
fn cell_entry_price(cell: usize) -> Price {
    Price::parse(&(75000 + cell * 1000).to_string()).unwrap()
}

/// 第 `cell` 格的买平价：开空价减一个价差 1000。
fn cell_exit_price(cell: usize) -> Price {
    Price::parse(&(74000 + cell * 1000).to_string()).unwrap()
}

/// 第 `batch` 批的主仓开空价：75000 往上每批 5000。
fn batch_price(batch: usize) -> Price {
    Price::parse(&(75000 + batch * 5000).to_string()).unwrap()
}

/// 评审给出的合约规则：5 位有效数字、步长下限 0.0001、数量步长 0.01、最低金额 10。
fn decade_spec() -> InstrumentSpec {
    InstrumentSpec::with_price_rule(
        btc(),
        PriceRule::SignificantFigures {
            max_figures: 5,
            step_floor: Price::parse("0.0001").unwrap(),
        },
        Quantity::parse("0.01").unwrap(),
        Notional::parse("10").unwrap(),
    )
    .unwrap()
}

/// 跨过 10 这一档的空头网格：`[9.9999, 10.1]`、`M = 100`、`d = .01`、`g = .1`，
/// 限速一小时。
fn throttled_short_across_a_decade() -> StrategyConfig {
    StrategyConfig {
        long: None,
        short: Some(SideConfig {
            price_low: Price::parse("9.9999").unwrap(),
            price_high: Price::parse("10.1").unwrap(),
            max_position: Quantity::parse("100").unwrap(),
            core_ratio: Rate::ZERO,
            batch_count: None,
            grid_spacing: Price::parse("0.01").unwrap(),
            grid_spread: Price::parse("0.1").unwrap(),
            throttle_interval: Some(Duration::hours(1)),
        }),
        ..common::long_grid_config()
    }
}

/// 价格写成十进制后去掉前导零剩下的位数：交易所最多收 5 位有效数字。
fn significant_figures(price: Price) -> usize {
    price
        .to_string()
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .trim_start_matches('0')
        .len()
}

/// B10：空头格跨过报价精度档位时逐格规整，开启限速后后面的格子照样按顺序放行。
///
/// 空头侧从 `L` 往上排，跨过 10 之后每一格都要落在 10 那一档的 `.001` 上：不规整的话
/// 第二格是 10.0099，交易所会拒掉这张单，这一格永远不成交，限速链条停在这里，后面的
/// 格子永远等不到放行。
#[tokio::test]
async fn b10_the_short_cells_across_a_decade_are_released_under_a_throttle() {
    let spec = decade_spec();
    let harness = harness_with_spec(
        throttled_short_across_a_decade(),
        spec.clone(),
        Store::open_in_memory().unwrap(),
    )
    .await;
    // 11 格：从 `L` 往上每格 `.01`，跨过 10 之后按 10 那一档的 `.001` 规整。
    let entries = [
        "9.9999", "10.0090", "10.0190", "10.0290", "10.0390", "10.0490", "10.0590", "10.0690",
        "10.0790", "10.0890", "10.0990",
    ];
    let hour = 3600;

    // 从 `L` 下面开始：第一格挂上去不成交，等价格涨上来。
    harness
        .exchange
        .set_market_price(Price::parse("9.99").unwrap(), at(0));
    let start = harness.step(0).await;

    assert_eq!(start.placed.len(), 1, "限速在启动时只放行第一格");
    assert_eq!(harness.side_state(Direction::Short).throttle().released, 1);

    // 价格一格一格涨上去：每满一个 τ 放行下一格，它在自己的开空价上当场开空。
    for (cell, entry) in entries.iter().enumerate() {
        let entry = Price::parse(entry).unwrap();
        let second = cell as i64 * hour;
        harness.exchange.set_market_price(entry, at(second));
        harness.step(second).await;

        assert_eq!(
            harness.side_state(Direction::Short).throttle().released,
            cell + 1,
            "第 {cell} 格应该已经放行"
        );
        assert!(
            harness.orders().iter().any(|order| {
                order.record.unit == short_cell(cell)
                    && order.record.purpose == OrderPurpose::GridEntry
                    && order.record.limit_price == entry
            }),
            "第 {cell} 格的开空单挂在规整后的 {entry}"
        );
    }

    // 每一张挂出去的单都落在它那个价位的步长上、不超过 5 位有效数字：交易所不收的价格
    // 会让这一格永远不成交，限速链条就停在那里。
    for order in harness.orders() {
        let price = order.record.limit_price;
        assert!(
            spec.price_rule().is_legal(price) && significant_figures(price) <= 5,
            "{:?} 挂在 {price}，交易所不收（该价位步长 {}，有效数字 {}）",
            order.record.unit,
            spec.price_rule().step_at(price),
            significant_figures(price)
        );
    }
    // 最后一格也放行并开空了：限速链条没有停在任何一格上。
    assert_eq!(harness.side_state(Direction::Short).throttle().released, 11);
    assert!(
        harness.cell_of(Direction::Short, 10).enabled,
        "最后一格也轮到了"
    );
}

/// B10：做空网格的完整过程，与 B01、B09 的数量结果一致、方向相反。
#[tokio::test]
async fn b10_a_rally_and_a_breakdown_build_the_core_short_and_cycle_the_cells() {
    let harness = core_short_harness().await;
    // 从 74 开始：四批主仓开空单挂在各自批次价，都不成交；格子全部未启用。
    harness
        .exchange
        .set_market_price(Price::parse("74000").unwrap(), at(0));
    let start = harness.step(0).await;

    assert_eq!(start.placed.len(), 4);
    assert_eq!(start.position, Position::ZERO);
    assert_eq!(start.expected_position, Position::ZERO);
    for batch in 0..4 {
        let order = harness
            .at_exchange(&harness.working_core_cloid_of(Direction::Short, batch))
            .unwrap();
        assert_eq!(order.limit_price, batch_price(batch));
        assert_eq!(order.quantity, Quantity::parse("0.175").unwrap());
        assert_eq!(
            harness.core_filled_of(Direction::Short, batch),
            Quantity::ZERO
        );
    }
    assert_eq!(harness.working().len(), 4);
    assert!(
        (0..10).all(|cell| !harness.cell_of(Direction::Short, cell).enabled),
        "no cell is enabled before its batch opens"
    );

    // 涨到 82：75、80 两批主仓开空，跟随它们的格子启用并按各自的开空价卖出。
    harness
        .exchange
        .set_market_price(Price::parse("82000").unwrap(), at(1));
    let rally = harness.step(1).await;

    assert_eq!(
        harness.core_filled_of(Direction::Short, 0),
        Quantity::parse("0.175").unwrap()
    );
    assert_eq!(
        harness.core_filled_of(Direction::Short, 1),
        Quantity::parse("0.175").unwrap()
    );
    assert_eq!(harness.core_filled_of(Direction::Short, 2), Quantity::ZERO);
    // 两批的 10 格全部启用，其中 75 到 82 共 8 格开空价不高于当前价、当场开空。
    assert_eq!(rally.placed.len(), 10);
    // 仓位核对发生在下单之前：那一刻交易所只有主仓空单 .35。
    assert_eq!(rally.position, rally.expected_position);
    assert_eq!(rally.position, Position::parse("-0.35").unwrap());
    // 8 格开完，交易所上的仓位是主仓 .35 加 8 格各 .015。
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("-0.47").unwrap()
    );
    for cell in 0..8 {
        assert!(harness.cell_of(Direction::Short, cell).enabled);
        assert_eq!(
            harness.cell_of(Direction::Short, cell).to_buy_back,
            Quantity::ZERO,
            "cell {cell} is fully short"
        );
    }
    // 83、84 两格已启用但还没轮到，挂着各自的卖单等价格上去。
    for cell in 8..10 {
        assert!(harness.cell_of(Direction::Short, cell).enabled);
        let sell = harness
            .at_exchange(&harness.working_cloid_of(Direction::Short, cell, OrderPurpose::GridEntry))
            .unwrap();
        assert_eq!(sell.limit_price, cell_entry_price(cell));
        assert!(!sell.reduce_only);
    }

    // 价格还在 82：开过空的 8 格按开空价减一个价差挂出买平单，单边网格标只减仓。
    let closes = harness.step(2).await;

    assert_eq!(closes.placed.len(), 8);
    assert_eq!(closes.position, Position::parse("-0.47").unwrap());
    assert_eq!(closes.expected_position, Position::parse("-0.47").unwrap());
    for cell in 0..8 {
        let buy = harness
            .at_exchange(&harness.working_cloid_of(Direction::Short, cell, OrderPurpose::GridExit))
            .unwrap();
        assert_eq!(buy.limit_price, cell_exit_price(cell));
        assert_eq!(buy.quantity, Quantity::parse("0.015").unwrap());
        assert!(buy.reduce_only, "a single-side grid closes reduce-only");
    }

    // 向下突破到 74：8 格全部按各自的买平价买回，并重新挂回各自的开空价。
    harness
        .exchange
        .set_market_price(Price::parse("74000").unwrap(), at(3));
    let breakdown = harness.step(3).await;

    // 买平单在价格回到 74 时就成交了，核对时交易所上只剩主仓空单。
    assert_eq!(breakdown.position, Position::parse("-0.35").unwrap());
    assert_eq!(
        breakdown.expected_position,
        Position::parse("-0.35").unwrap()
    );
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("-0.35").unwrap()
    );
    for cell in 0..8 {
        assert_eq!(
            harness.cell_of(Direction::Short, cell).to_buy_back,
            Quantity::parse("0.015").unwrap(),
            "cell {cell} is flat again"
        );
        let sell = harness
            .at_exchange(&harness.working_cloid_of(Direction::Short, cell, OrderPurpose::GridEntry))
            .unwrap();
        assert_eq!(sell.limit_price, cell_entry_price(cell));
    }
    // 只剩主仓空单 .35，加上 10 格的开空单和 2 批还没成交的主仓开空单。
    assert_eq!(harness.working().len(), 12);
    // 主仓只开空不买平：全过程没有一张买回主仓的订单。
    assert!(
        harness
            .orders()
            .iter()
            .filter(|order| matches!(order.record.unit, Unit::Batch(..)))
            .all(|order| order.record.unit.side(order.record.purpose) == Side::Sell),
        "the core is never bought back"
    );
    assert!(
        harness
            .orders()
            .iter()
            .all(|order| order.record.unit.direction() == Direction::Short),
        "a single-side short grid never places a long order"
    );

    // 再走一步不会重复挂单。
    let idle = harness.step(4).await;
    assert!(idle.placed.is_empty());
    assert!(idle.canceled.is_empty());
    assert_eq!(idle.position, Position::parse("-0.35").unwrap());
}

/// B12：启动预览里将要立即开空的那几格，确认之后确实按顺序开了空。
#[tokio::test]
async fn b12_the_preview_and_the_first_step_open_the_same_short_cells() {
    let harness = neutral_harness().await;
    harness
        .exchange
        .set_market_price(Price::parse("93").unwrap(), at(0));

    // 网页上的预览路径：新建草稿再看预览，确认之前一张单都不下。
    let draft_id = harness.runtime.create_draft(neutral_grid_config()).unwrap();
    let preview = harness.runtime.preview_draft(&draft_id).await.unwrap();

    assert_eq!(preview.market_price, Price::parse("93").unwrap());
    assert!(preview.batches.is_empty());
    let expected: Vec<Unit> = (0..3).map(short_cell).collect();
    assert_eq!(
        preview
            .immediate_fills
            .iter()
            .map(|fill| fill.unit)
            .collect::<Vec<Unit>>(),
        expected
    );
    for (index, fill) in preview.immediate_fills.iter().enumerate() {
        assert_eq!(fill.purpose, OrderPurpose::GridEntry);
        assert_eq!(
            fill.limit_price,
            Price::parse(&(91 + index).to_string()).unwrap()
        );
        assert_eq!(fill.quantity, Quantity::parse("0.05").unwrap());
    }

    // 确认后走一步：两侧的 15 张单一起挂出，越过当前价的三格当场开空。
    let start = harness.step(0).await;

    assert_eq!(start.placed.len(), 15);
    assert_eq!(start.position, Position::ZERO);
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("-0.15").unwrap()
    );
    // 当场成交的三格立刻记成已开空，另外两格还在等价格上去。
    for cell in 0..3 {
        assert_eq!(
            harness.cell_of(Direction::Short, cell).to_buy_back,
            Quantity::ZERO,
            "short cell {cell} opened immediately"
        );
    }
    for cell in 3..5 {
        assert_eq!(
            harness.cell_of(Direction::Short, cell).to_buy_back,
            Quantity::parse("0.05").unwrap()
        );
    }

    // 下一步按顺序给这三格挂出买平单。
    let after = harness.step(1).await;

    assert_eq!(after.position, Position::parse("-0.15").unwrap());
    assert_eq!(after.position, after.expected_position);
    assert_eq!(after.placed.len(), 3);
    for cell in 0..3 {
        let close = harness
            .at_exchange(&harness.working_cloid_of(Direction::Short, cell, OrderPurpose::GridExit))
            .unwrap();
        assert_eq!(
            close.limit_price,
            Price::parse(&(90 + cell).to_string()).unwrap()
        );
    }
}

/// B11：中性网格两侧各自做 T，净仓位不超过两侧上限，格子单不标只减仓。
#[tokio::test]
async fn b11_a_neutral_grid_cycles_both_sides_within_the_two_limits() {
    let harness = neutral_harness().await;
    // 价格 90 落在两侧之间的空隙里：两侧的单都挂着，谁也不成交。
    harness
        .exchange
        .set_market_price(Price::parse("90").unwrap(), at(0));
    let start = harness.step(0).await;

    assert_eq!(start.placed.len(), 15, "10 long cells and 5 short cells");
    assert_eq!(start.position, Position::ZERO);
    assert_eq!(start.expected_position, Position::ZERO);
    assert!(
        harness
            .working()
            .iter()
            .all(|order| !order.record.reduce_only)
    );

    // 涨到 93：空头侧 91、92、93 三格开空，各自挂出买平单。
    harness
        .exchange
        .set_market_price(Price::parse("93").unwrap(), at(1));
    let up = harness.step(1).await;

    assert_eq!(up.position, Position::parse("-0.15").unwrap());
    assert_eq!(up.position, up.expected_position);
    for cell in 0..3 {
        assert_eq!(
            harness.cell_of(Direction::Short, cell).to_buy_back,
            Quantity::ZERO
        );
        let close = harness
            .at_exchange(&harness.working_cloid_of(Direction::Short, cell, OrderPurpose::GridExit))
            .unwrap();
        assert_eq!(
            close.limit_price,
            Price::parse(&(90 + cell).to_string()).unwrap()
        );
        // 中性网格的平仓单不标只减仓：它在交易所层面可能是在减另一侧的仓位。
        assert!(!close.reduce_only);
    }
    // 多头侧一格都没成交。
    assert!(
        harness
            .orders()
            .iter()
            .filter(|order| order.record.unit.direction() == Direction::Long)
            .all(|order| order.record.purpose == OrderPurpose::GridEntry)
    );

    // 涨到 96：空头侧五格全部开空，正好是空头侧的上限 .25。
    harness
        .exchange
        .set_market_price(Price::parse("96").unwrap(), at(2));
    let top = harness.step(2).await;

    assert_eq!(top.position, Position::parse("-0.25").unwrap());
    assert_eq!(top.position, top.expected_position);
    assert_eq!(
        harness.exchange.current_position(),
        Position::parse("-0.25").unwrap()
    );

    // 跌回 90：空头侧五格全部买平，回到零；多头侧还没轮到。
    harness
        .exchange
        .set_market_price(Price::parse("90").unwrap(), at(3));
    let back = harness.step(3).await;

    assert_eq!(back.position, Position::ZERO);
    assert_eq!(back.position, back.expected_position);
    assert_eq!(harness.exchange.current_position(), Position::ZERO);
    // 空头侧走完一轮：五格各自开空又买平，重新挂回开空价。
    for cell in 0..5 {
        assert_eq!(
            harness.cell_of(Direction::Short, cell).to_buy_back,
            Quantity::parse("0.05").unwrap()
        );
        let sell = harness
            .at_exchange(&harness.working_cloid_of(Direction::Short, cell, OrderPurpose::GridEntry))
            .unwrap();
        assert_eq!(
            sell.limit_price,
            Price::parse(&(91 + cell).to_string()).unwrap()
        );
    }

    // 跌到 85：多头侧 89 到 85 五格买入，净仓位 +.5。
    harness
        .exchange
        .set_market_price(Price::parse("85").unwrap(), at(4));
    let down = harness.step(4).await;

    assert_eq!(down.position, Position::parse("0.5").unwrap());
    assert_eq!(down.position, down.expected_position);
    for cell in 0..5 {
        let sell = harness
            .at_exchange(&harness.working_cloid_of(Direction::Long, cell, OrderPurpose::GridExit))
            .unwrap();
        assert_eq!(
            sell.limit_price,
            Price::parse(&(90 - cell).to_string()).unwrap()
        );
        assert!(
            !sell.reduce_only,
            "a neutral grid closes without reduce-only"
        );
    }

    // 涨回 96：多头侧五格卖出、空头侧五格开空，净仓位回到空头侧的上限。
    harness
        .exchange
        .set_market_price(Price::parse("96").unwrap(), at(5));
    let round_trip = harness.step(5).await;

    assert_eq!(round_trip.position, Position::parse("-0.25").unwrap());
    assert_eq!(round_trip.position, round_trip.expected_position);
    // 两侧都走完了至少一轮 T：两侧都有成交过的平仓单。
    for direction in Direction::ALL {
        assert!(
            harness.orders().iter().any(|order| {
                order.record.unit.direction() == direction
                    && order.record.purpose == OrderPurpose::GridExit
                    && order.record.cumulative_filled > Quantity::ZERO
            }),
            "the {direction} side completed a round trip"
        );
    }
    // 中性网格全程没有一张只减仓的订单。
    assert!(
        harness
            .orders()
            .iter()
            .all(|order| !order.record.reduce_only),
        "a neutral grid never marks an order reduce-only"
    );

    // 再走一步不会重复挂单。
    let idle = harness.step(6).await;
    assert!(idle.placed.is_empty());
    assert!(idle.canceled.is_empty());
    assert_eq!(idle.position, Position::parse("-0.25").unwrap());
}

/// 净仓位始终落在两侧上限之间：多头侧最多 +1，空头侧最多 −.25。
///
/// 单独走一遍价格路径，每一步都核对上界，而不是只看终点。
#[tokio::test]
async fn b11_the_net_position_stays_within_the_two_sides_limits() {
    let harness = neutral_harness().await;
    let long_limit = Position::parse("1").unwrap();
    let short_limit = Position::parse("-0.25").unwrap();
    let path = ["90", "93", "96", "90", "85", "79", "96", "78", "96"];

    for (second, price) in path.iter().enumerate() {
        harness
            .exchange
            .set_market_price(Price::parse(price).unwrap(), at(second as i64));
        let report = harness.step(second as i64).await;

        assert_eq!(report.position, report.expected_position);
        assert!(
            report.position <= long_limit && report.position >= short_limit,
            "step {second} at {price} left the net position at {}",
            report.position
        );
        // 敞口也不会超过两侧上限之和。
        assert_eq!(harness.exchange.current_position(), report.position);
    }
    // 最后一步在 96：两侧的平仓单都成交了，两侧的格子都回到「等下一次建仓」。
    for cell in 0..10 {
        assert_eq!(
            harness.cell_of(Direction::Long, cell).to_buy_back,
            Quantity::parse("0.1").unwrap(),
            "long cell {cell} is flat again"
        );
    }
    // 空头侧正好相反：最后一步在 96，五格全部开空、等着买平，仓位就是空头侧的上限。
    for cell in 0..5 {
        assert_eq!(
            harness.cell_of(Direction::Short, cell).to_buy_back,
            Quantity::ZERO,
            "short cell {cell} is short"
        );
    }
    assert_eq!(harness.exchange.current_position(), short_limit);
}
