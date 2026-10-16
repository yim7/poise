//! 观点失效（第 7.5 节）：什么时候触发，触发之后这一次补发多少减仓。
//!
//! 只做计算。撤单、发单、等订单有结论由 runtime 负责。减仓数量按交易所实时查询的仓位
//! 算，不按账上的状态：成交推送断开、成交记录还没到的时候，账上的状态是旧的（B39）。

use crate::Strategy;
use crate::batch::BatchIndex;
use crate::order::{OrderPurpose, OrderRecord, Side, Unit, WorkingOrder};
use crate::planning::OrderIntent;
use crate::side::Direction;
use crate::state::StrategyState;
use crate::units::{Position, Price, PriceRounding, Quantity, Rate, SCALE};

/// 观点失效每一侧已经卖出（或买回）的数量：失效减仓单记入的累计成交量之和，按侧分开。
///
/// 和 [`Strategy::after_invalidation`] 配合使用，给批次表、格子表算失效之后的持有。
pub fn invalidation_sold(orders: &[OrderRecord]) -> Vec<(Direction, Quantity)> {
    Direction::ALL
        .into_iter()
        .map(|direction| {
            let sold = orders
                .iter()
                .filter(|order| {
                    order.purpose == OrderPurpose::Invalidation
                        && order.unit.direction() == direction
                })
                .fold(Quantity::ZERO, |total, order| {
                    total + order.cumulative_filled
                });
            (direction, sold)
        })
        .collect()
}

/// 正整数除法，向上取整。
fn div_ceil(numerator: i128, denominator: i128) -> i128 {
    (numerator + denominator - 1).div_euclid(denominator)
}

/// 这一次的减仓规划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduction {
    /// 实时仓位超出剩余数量的部分；没有超出是 0。
    pub excess: Quantity,
    /// 超出部分已经无法下单：是 0，或者不足一个数量步长、按标记价不足最低下单金额。
    /// 这时减仓这一项算完成，`excess` 就是界面上显示的尾量（B39d）。
    pub settled: bool,
    /// 这一次要补发的减仓单。超出部分已经被还可能成交的减仓单覆盖、或者补发量不足最低
    /// 下单金额时是 `None`。
    pub order: Option<OrderIntent>,
}

impl Strategy {
    /// 标记价越过了哪一侧的失效价：多头侧是低于失效价，空头侧是高于失效价。两侧都没设
    /// 失效价，或者都没越过，是 `None`。
    pub fn invalidation_triggered(&self, mark: Price) -> Option<Direction> {
        self.sides().iter().find_map(|side| {
            let invalidation = side.config().invalidation?;
            let crossed = match side.direction() {
                Direction::Long => mark < invalidation.price,
                Direction::Short => mark > invalidation.price,
            };
            crossed.then_some(side.direction())
        })
    }

    /// 失效之后这一次要补发的减仓单（第 7.5 节第 3 步）。
    ///
    /// 单侧网格把本侧仓位减到剩余数量 R；中性网格任一侧失效都全部平仓（B40），持有的是
    /// 哪一侧就减哪一侧。补发数量 = 实时仓位超出 R 的部分 − 本策略还可能成交的减仓单余量
    /// （`working` 里同方向的单，含结果未知的，B39c），按数量步长向下取整。
    ///
    /// 限价相对标记价最多让出滑点上限：卖出向上取到报价精度，买入向下取，规整之后也
    /// 不会让得更多。卖出让满滑点上限之后不足最低下单金额时，限价只让到刚好够最低金额
    /// 为止（滑点上限是最多让多少，不是必须让满）。
    ///
    /// 超出部分按**标记价**不足最低下单金额（或不足一个数量步长）就算减仓完成（第 7.5 节
    /// 第 4 步）：那部分下不了单，界面显示尾量。
    pub fn reduction(
        &self,
        position: Position,
        mark: Price,
        working: &[WorkingOrder],
    ) -> Reduction {
        let (holding, excess) = self.excess(position);
        let spec = self.spec();
        let Some(holding) =
            holding.filter(|_| spec.meets_minimum_order(spec.normalize_quantity(excess), mark))
        else {
            return Reduction {
                excess,
                settled: true,
                order: None,
            };
        };

        let side = holding.exit_side();
        let pending = working
            .iter()
            .filter(|order| order.unit.side(order.purpose) == side)
            .fold(Quantity::ZERO, |total, order| total + order.remaining);
        let quantity = spec.normalize_quantity(excess.saturating_sub(pending).max(Quantity::ZERO));
        let order = self
            .reduction_price(holding, mark, side, quantity)
            .map(|limit_price| OrderIntent {
                unit: Unit::Side(holding),
                purpose: OrderPurpose::Invalidation,
                side,
                limit_price,
                quantity,
                reduce_only: true,
            });
        Reduction {
            excess,
            settled: false,
            order,
        }
    }

    /// 批次表和格子表要显示的状态：把观点失效已经卖出的数量按「先网格、后主仓」从账上
    /// 扣掉（第 7.5 节）。网格从离空隙最远的格子减起，主仓从最后一批减起，减到零为止。
    ///
    /// 只用于显示。失效减仓的成交不进账本（见 [`Strategy::apply_fill`]），这里每次按
    /// 已卖出的总量现算，结果和成交到达的先后无关。
    pub fn after_invalidation(
        &self,
        state: &StrategyState,
        sold: &[(Direction, Quantity)],
    ) -> StrategyState {
        let mut state = state.clone();
        for (direction, quantity) in sold {
            let Some(plan) = self.side(*direction) else {
                continue;
            };
            let Some(side) = state.side_mut(*direction) else {
                continue;
            };
            let mut left = *quantity;
            for cell in plan.grid().cells().iter().rev() {
                let Some(cell_state) = side.cell_mut(cell.index) else {
                    continue;
                };
                let taken = cell_state.held(cell.quantity).min(left);
                cell_state.to_buy_back = cell_state.to_buy_back.saturating_add(taken);
                left = left.saturating_sub(taken);
            }
            for index in (0..side.batches().len()).rev() {
                let Some(batch) = side.batch_mut(BatchIndex(index)) else {
                    continue;
                };
                let taken = batch.filled.min(left);
                batch.filled = batch.filled.saturating_sub(taken);
                left = left.saturating_sub(taken);
            }
        }
        state
    }

    /// 实时仓位超出剩余数量的部分，以及持有的是哪一侧。没有超出时持有侧是 `None`。
    fn excess(&self, position: Position) -> (Option<Direction>, Quantity) {
        let (holding, remaining) = if self.is_neutral() {
            let holding = match position.units() {
                units if units > 0 => Direction::Long,
                units if units < 0 => Direction::Short,
                _ => return (None, Quantity::ZERO),
            };
            (holding, Quantity::ZERO)
        } else {
            let Some(side) = self.sides().first() else {
                return (None, Quantity::ZERO);
            };
            let remaining = side
                .config()
                .invalidation
                .map_or(Quantity::ZERO, |invalidation| invalidation.remaining);
            (side.direction(), remaining)
        };
        let held = Quantity::from_units((position.units() * holding.sign()).max(0));
        // `saturating_sub` 不在零处截断：仓位不超过 R 时差是负数。
        let excess = held.saturating_sub(remaining).max(Quantity::ZERO);
        if excess.is_zero() {
            (None, Quantity::ZERO)
        } else {
            (Some(holding), excess)
        }
    }

    /// 减仓单的限价：标记价让出滑点上限。中性网格持有侧没设失效时用另一侧的滑点上限。
    ///
    /// 数量为零、或者按标记价都不够最低下单金额时是 `None`：这一次不发单。
    fn reduction_price(
        &self,
        holding: Direction,
        mark: Price,
        side: Side,
        quantity: Quantity,
    ) -> Option<Price> {
        let spec = self.spec();
        if !spec.meets_minimum_order(quantity, mark) {
            return None;
        }
        let slippage = self
            .side(holding)
            .and_then(|plan| plan.config().invalidation)
            .or_else(|| {
                self.sides()
                    .iter()
                    .find_map(|plan| plan.config().invalidation)
            })
            .map_or(Rate::ZERO, |invalidation| invalidation.max_slippage);
        let scale = i128::from(SCALE);
        let (factor, rounding) = match side {
            Side::Sell => (scale - i128::from(slippage.units()), PriceRounding::Up),
            Side::Buy => (scale + i128::from(slippage.units()), PriceRounding::Down),
        };
        let product = i128::from(mark.units()) * factor;
        // 卖出向上、买入向下：先按同一方向除，再按同一方向规整到报价精度。
        let raw = match rounding {
            PriceRounding::Up => (product + scale - 1).div_euclid(scale),
            _ => product.div_euclid(scale),
        };
        let price = Price::from_units(i64::try_from(raw).ok()?);
        if price.units() <= 0 {
            return None;
        }
        let slipped = spec.normalize_price(price, rounding)?;
        if spec.meets_minimum_order(quantity, slipped) {
            return Some(slipped);
        }
        // 卖出让满之后不够最低金额：限价抬到刚好够的那个价位，不超过标记价。买入的限价
        // 高于标记价，按标记价够了就一定够，走不到这里。
        let floor = div_ceil(
            spec.min_order_notional().raw(),
            i128::from(quantity.units()),
        );
        let floor = spec.normalize_price(
            Price::from_units(i64::try_from(floor).ok()?),
            PriceRounding::Up,
        )?;
        (side == Side::Sell && floor <= mark && spec.meets_minimum_order(quantity, floor))
            .then_some(floor)
    }
}

#[cfg(test)]
mod tests {
    use crate::Strategy;
    use crate::batch::BatchIndex;
    use crate::config::{Invalidation, StrategyConfig};
    use crate::fixtures::{
        btc_fees, btc_spec, cheap_spec, core_long_config, core_short_config, neutral_grid_config,
    };
    use crate::grid::CellIndex;
    use crate::order::{ClientOrderId, OrderPurpose, Side, Unit, WorkingOrder};
    use crate::side::Direction;
    use crate::units::{Position, Price, Quantity, Rate};

    fn invalidation(price: &str, remaining: &str) -> Invalidation {
        Invalidation {
            price: Price::parse(price).unwrap(),
            remaining: Quantity::parse(remaining).unwrap(),
            max_slippage: Rate::parse("0.01").unwrap(),
        }
    }

    fn core_long(remaining: &str) -> Strategy {
        let mut config = core_long_config();
        config.long.as_mut().unwrap().invalidation = Some(invalidation("70000", remaining));
        Strategy::new(config, btc_spec(), btc_fees()).unwrap()
    }

    /// 带符号的仓位：`Position::parse` 只认非负数，空头仓位按数量取反。
    fn position(value: &str) -> Position {
        match value.strip_prefix('-') {
            Some(short) => Position::from_units(-Position::parse(short).unwrap().units()),
            None => Position::parse(value).unwrap(),
        }
    }

    fn price(value: &str) -> Price {
        Price::parse(value).unwrap()
    }

    fn quantity(value: &str) -> Quantity {
        Quantity::parse(value).unwrap()
    }

    fn unknown_sell(remaining: &str) -> WorkingOrder {
        WorkingOrder {
            client_order_id: ClientOrderId::try_new("0xa").unwrap(),
            unit: Unit::Side(Direction::Long),
            purpose: OrderPurpose::Invalidation,
            limit_price: price("69000"),
            remaining: quantity(remaining),
        }
    }

    /// 第 7.5 节第 1 步：多头侧低于失效价才算越过，正好等于不算。
    #[test]
    fn a_long_side_triggers_below_the_invalidation_price() {
        let strategy = core_long("0.3");

        assert_eq!(strategy.invalidation_triggered(price("70000")), None);
        assert_eq!(
            strategy.invalidation_triggered(price("69999")),
            Some(Direction::Long)
        );
    }

    #[test]
    fn a_short_side_triggers_above_the_invalidation_price() {
        let mut config = core_short_config();
        config.short.as_mut().unwrap().invalidation = Some(invalidation("100000", "0"));
        let strategy = Strategy::new(config, btc_spec(), btc_fees()).unwrap();

        assert_eq!(strategy.invalidation_triggered(price("100000")), None);
        assert_eq!(
            strategy.invalidation_triggered(price("100001")),
            Some(Direction::Short)
        );
    }

    #[test]
    fn nothing_triggers_without_an_invalidation_price() {
        let strategy = Strategy::new(core_long_config(), btc_spec(), btc_fees()).unwrap();

        assert_eq!(strategy.invalidation_triggered(price("1")), None);
    }

    /// B37：持仓 1、R = .3：卖出 .7，只减仓，限价比标记价最多低 1%，向上取到报价精度。
    #[test]
    fn b37_reduces_the_position_down_to_the_remaining_quantity() {
        let strategy = core_long("0.3");

        let reduction = strategy.reduction(position("1"), price("69001"), &[]);

        assert_eq!(reduction.excess, quantity("0.7"));
        assert!(!reduction.settled);
        let order = reduction.order.unwrap();
        assert_eq!(order.unit, Unit::Side(Direction::Long));
        assert_eq!(order.purpose, OrderPurpose::Invalidation);
        assert_eq!(order.side, Side::Sell);
        assert!(order.reduce_only);
        assert_eq!(order.quantity, quantity("0.7"));
        // 69001 × .99 = 68310.99，向上取到 1 美元：68311，没有让出超过 1%。
        assert_eq!(order.limit_price, price("68311"));
    }

    /// B39c：持仓 1、R = .7；减仓单 A 卖 .3 结果未知：补发数量 = 1 − .7 − .3 = 0。
    #[test]
    fn b39c_pending_reduce_orders_are_not_sold_again() {
        let strategy = core_long("0.7");

        let reduction = strategy.reduction(position("1"), price("69000"), &[unknown_sell("0.3")]);

        assert_eq!(reduction.excess, quantity("0.3"));
        assert!(!reduction.settled);
        assert_eq!(reduction.order, None);
    }

    /// 同方向的单才算减仓单：一张没撤掉的买单不会让减仓少卖。
    #[test]
    fn buy_orders_do_not_count_as_pending_reductions() {
        let strategy = core_long("0.7");
        let buy = WorkingOrder {
            unit: Unit::Batch(Direction::Long, BatchIndex(0)),
            purpose: OrderPurpose::Core,
            ..unknown_sell("0.3")
        };

        let reduction = strategy.reduction(position("1"), price("69000"), &[buy]);

        assert_eq!(reduction.order.unwrap().quantity, quantity("0.3"));
    }

    /// B39d：仓位 .70001、R = .7：超出部分不足一个数量步长，算完成，尾量 .00001。
    #[test]
    fn b39d_an_excess_below_the_minimum_order_is_settled_with_a_tail() {
        let strategy = core_long("0.7");

        let reduction = strategy.reduction(position("0.70001"), price("69000"), &[]);

        assert!(reduction.settled);
        assert_eq!(reduction.excess, quantity("0.00001"));
        assert_eq!(reduction.order, None);
    }

    /// 完成按标记价算（第 7.5 节第 4 步）：超出 .001，标记价 10000 时正好 10 美元，够最低
    /// 金额，不算完成；让满 1% 的限价 9900 不够，限价只让到刚好够的 10000。标记价 9999
    /// 时不够，算完成。
    #[test]
    fn the_minimum_notional_is_judged_at_the_mark_price() {
        let strategy = core_long("0.7");

        let boundary = strategy.reduction(position("0.701"), price("10000"), &[]);
        assert!(!boundary.settled);
        let order = boundary.order.unwrap();
        assert_eq!(order.quantity, quantity("0.001"));
        assert_eq!(order.limit_price, price("10000"));

        let tail = strategy.reduction(position("0.701"), price("9999"), &[]);
        assert!(tail.settled);
        assert_eq!(tail.excess, quantity("0.001"));
        assert_eq!(tail.order, None);
    }

    /// 批次表和格子表按「先网格、后主仓」显示失效之后的持有：网格从离空隙最远的格子
    /// 减起，主仓从最后一批减起。
    #[test]
    fn the_tables_show_the_grid_reduced_before_the_core() {
        let strategy = core_long("0.3");
        let mut state = strategy.initial_state();
        let side = state.side_mut(Direction::Long).unwrap();
        side.batch_mut(BatchIndex(0)).unwrap().filled = quantity("0.175");
        side.batch_mut(BatchIndex(1)).unwrap().filled = quantity("0.1");
        for index in [0, 1] {
            let cell = side.cell_mut(CellIndex(index)).unwrap();
            cell.enabled = true;
            cell.to_buy_back = Quantity::ZERO;
        }
        assert_eq!(strategy.expected_position(&state), position("0.305"));

        let shown = strategy.after_invalidation(&state, &[(Direction::Long, quantity("0.1"))]);

        let side = shown.side(Direction::Long).unwrap();
        for index in [0, 1] {
            assert_eq!(
                side.cell(CellIndex(index)).unwrap().to_buy_back,
                quantity("0.015")
            );
        }
        assert_eq!(side.batch(BatchIndex(0)).unwrap().filled, quantity("0.175"));
        assert_eq!(side.batch(BatchIndex(1)).unwrap().filled, quantity("0.03"));
        assert_eq!(strategy.expected_position(&shown), position("0.205"));
        // 卖得比账上还多时减到零为止，不记成负数。
        let all = strategy.after_invalidation(&state, &[(Direction::Long, quantity("1"))]);
        assert_eq!(strategy.expected_position(&all), Position::ZERO);
    }

    #[test]
    fn a_position_at_or_below_the_remaining_quantity_is_settled() {
        let strategy = core_long("0.3");

        for held in ["0.3", "0.1", "0", "-0.2"] {
            let reduction = strategy.reduction(position(held), price("69000"), &[]);
            assert!(reduction.settled, "{held}");
            assert_eq!(reduction.excess, Quantity::ZERO, "{held}");
            assert_eq!(reduction.order, None, "{held}");
        }
    }

    /// B40：中性网格全部平仓。净空头时买回，限价比标记价最多高 1%，向下取到报价精度。
    #[test]
    fn b40_a_neutral_grid_flattens_whichever_side_it_holds() {
        let mut config: StrategyConfig = neutral_grid_config();
        config.short.as_mut().unwrap().invalidation = Some(Invalidation {
            price: price("97"),
            remaining: Quantity::ZERO,
            max_slippage: Rate::parse("0.01").unwrap(),
        });
        let strategy = Strategy::new(config, cheap_spec(), btc_fees()).unwrap();

        let short = strategy.reduction(position("-0.15"), price("98"), &[]);
        let order = short.order.unwrap();
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.unit, Unit::Side(Direction::Short));
        assert_eq!(order.quantity, quantity("0.15"));
        assert!(order.limit_price <= price("98.98"), "{}", order.limit_price);

        // 持有的是多头侧：同样全部卖出，滑点上限借用设了失效的那一侧。
        let long = strategy.reduction(position("0.3"), price("98"), &[]);
        let order = long.order.unwrap();
        assert_eq!(order.side, Side::Sell);
        assert_eq!(order.unit, Unit::Side(Direction::Long));
        assert_eq!(order.quantity, quantity("0.3"));
        assert!(order.limit_price >= price("97.02"), "{}", order.limit_price);
    }
}
