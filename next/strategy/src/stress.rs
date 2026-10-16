//! 满仓压力测试（第 7.3 节）：假设一侧所有批次和格子都已成交、价格到达最不利的位置，
//! 估算账户保证金率和强平价。
//!
//! 纯计算。账户余额、其他仓位的维持保证金、标的的保证金分档表由调用方从交易所读来。
//! 这是基于当前账户状态的估算，不是保证：其他仓位和余额变化后，以运行时的保证金率
//! 闸门为准（第 7.4 节）。
//!
//! 维持保证金按 Hyperliquid 的分档公式（官方「Margin tiers」）：
//! `名义价值 × 维持保证金率 − 扣减额`，每一档的维持保证金率是该档最大杠杆下初始保证金率
//! 的一半，扣减额逐档累加，使维持保证金随名义价值连续变化。

use serde::{Deserialize, Serialize};

use crate::Strategy;
use crate::instrument::Fees;
use crate::side::{Direction, SidePlan};
use crate::state::StrategyState;
use crate::units::{Notional, Position, Price, Quantity, Rate, SCALE};

/// 保证金分档表的一档：名义价值不低于 `lower_bound` 时，最大杠杆是 `max_leverage`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginTier {
    pub lower_bound: Notional,
    pub max_leverage: u32,
}

/// 一个标的的保证金分档表，按下限从低到高。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginTiers {
    tiers: Vec<MarginTier>,
}

/// 分档表不合法。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarginTiersError {
    #[error("the margin tiers must start at a zero lower bound and rise strictly")]
    BadBounds,
    #[error("every margin tier needs a positive max leverage")]
    ZeroLeverage,
}

impl MarginTiers {
    pub fn new(tiers: Vec<MarginTier>) -> Result<Self, MarginTiersError> {
        if tiers.first().is_none_or(|tier| !tier.lower_bound.is_zero())
            || tiers
                .windows(2)
                .any(|pair| pair[1].lower_bound <= pair[0].lower_bound)
        {
            return Err(MarginTiersError::BadBounds);
        }
        if tiers.iter().any(|tier| tier.max_leverage == 0) {
            return Err(MarginTiersError::ZeroLeverage);
        }
        Ok(Self { tiers })
    }

    /// 只有一档：没有分档表的标的。
    pub fn single(max_leverage: u32) -> Self {
        Self {
            tiers: vec![MarginTier {
                lower_bound: Notional::ZERO,
                max_leverage: max_leverage.max(1),
            }],
        }
    }

    /// 一笔名义价值的维持保证金：`名义价值 ÷ (2 × 该档最大杠杆) − 扣减额`。
    ///
    /// 扣减额是 `Σ 第 k 档下限 × (第 k 档费率 − 第 k−1 档费率)`，k 从 1 到所在档。按 1e-16
    /// 的整数算，取整一律往维持保证金偏大的方向：前一项向上取整、扣减额向下取整，最后再
    /// 向上取整到 1e-8。压力测试宁可高估占用。
    pub fn maintenance_margin(&self, notional: Notional) -> Notional {
        let notional = notional.raw().max(0);
        let mut deduction = 0_i128;
        let mut rate_divisor = 0_i128; // 当前档的 2 × 最大杠杆
        for (index, tier) in self.tiers.iter().enumerate() {
            if index > 0 && notional < tier.lower_bound.raw() {
                break;
            }
            let divisor = 2 * i128::from(tier.max_leverage);
            if index > 0 {
                // 下限 × (1/divisor − 1/上一档 divisor)：往小里取。
                let lower = tier.lower_bound.raw();
                deduction += lower / divisor - ceil_div(lower, rate_divisor);
            }
            rate_divisor = divisor;
        }
        notional_ceil(ceil_div(notional, rate_divisor) - deduction.max(0))
    }
}

/// 压力测试时账户的底数：当前读到的余额和别处的占用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountBasis {
    /// USDC 余额。
    pub balance: Notional,
    /// 逐仓仓位占用的保证金。
    pub isolated_margin: Notional,
    /// 这个标的之外的全仓维持保证金占用，按当前标记价。
    pub other_maintenance: Notional,
}

/// 一侧的压力测试结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideStress {
    pub direction: Direction,
    /// 最不利的价格：做多是范围下沿，做空是范围上沿。
    pub adverse_price: Price,
    /// 满仓数量，就是这一侧的 `M`。
    pub position: Quantity,
    /// 满仓时按各批、各格的挂单价算的持仓均价。
    pub average_entry: Price,
    /// 满仓、价格在最不利位置时的账户保证金率。账户权益不为正时报 100 倍，表示早已强平。
    pub margin_ratio: Rate,
    /// 满仓时会不会强平、在哪里强平。
    pub liquidation: Liquidation,
}

/// 满仓时的强平结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Liquidation {
    /// 价格怎么走都不会强平：做多到价格为零，权益还盖得住维持保证金。
    Never,
    /// 价格到这里就会强平。
    At { price: Price },
    /// 强平价超出能表示的价格范围，算不出来。做空总会在某个价格强平，只是太远了。
    OutOfRange,
}

/// 保证金率的上限：权益不为正时报这个数（10000%），表示早已强平。
const RATIO_CAP_UNITS: i64 = 100 * SCALE;

impl Strategy {
    /// 两侧各自满仓的压力测试，按 [`Direction::ALL`] 的顺序（第 7.3 节）。
    ///
    /// `start` 是启动时的状态（接管已有仓位时是 [`Strategy::takeover_state`]）。已经持有的
    /// 那部分按当前标记价 `mark_price` 计成本——它的真实成本不一定是计划价，从现在跌到最
    /// 不利价格的亏损才是要扛的；还没成交的部分按各批、各格的挂单价计。
    ///
    /// 中性网格两侧分开算：多头侧在下方满仓、空头侧在上方满仓，不让两侧数量相互抵消。
    pub fn stress_test(
        &self,
        start: &StrategyState,
        mark_price: Price,
        basis: &AccountBasis,
        tiers: &MarginTiers,
    ) -> Vec<SideStress> {
        self.sides()
            .iter()
            .map(|side| {
                let full = FullPosition::of(side, start, mark_price, self.fees());
                let adverse_price = adverse_price(side);
                SideStress {
                    direction: side.direction(),
                    adverse_price,
                    position: full.quantity(),
                    average_entry: full.average_entry(),
                    margin_ratio: full.margin_ratio(basis, tiers, adverse_price),
                    liquidation: full.liquidation(basis, tiers),
                }
            })
            .collect()
    }

    /// 这一侧把 `M` 改成多少，满仓保证金率才低于 `limit`：按改过 `M` 的配置真正重新划分
    /// 批次和格子、接管 `position`，再做一遍压力测试——按比例缩放会忽略重新划分时的数量
    /// 取整，算出来的建议照着填进去可能还是超限。按数量精度向下取整。
    ///
    /// 两次二分，检查次数都是对数级：
    /// 1. 先找最小的合法 `M`。`M` 太小时每格不够最低下单金额，配置不合法；`M` 越大越合法。
    /// 2. 再在合法区间里找最大的达标 `M`：仓位越大保证金率越高。
    pub fn suggest_max_position(
        &self,
        direction: Direction,
        position: Position,
        mark_price: Price,
        basis: &AccountBasis,
        tiers: &MarginTiers,
        limit: Rate,
    ) -> Suggestion {
        let Some(side) = self.side(direction) else {
            return Suggestion::NoFeasiblePosition;
        };
        let step = self.spec().quantity_step().units().max(1);
        let held = match direction {
            Direction::Long => position.units().max(0),
            Direction::Short => (-position.units()).max(0),
        };
        // `None`：这个 `M` 的配置不合法或者接管不了；`Some(达标与否)`：做完压力测试的结论。
        let check = |steps: i64| -> Option<bool> {
            let mut config = self.config().clone();
            let side_config = match direction {
                Direction::Long => config.long.as_mut()?,
                Direction::Short => config.short.as_mut()?,
            };
            side_config.max_position = Quantity::from_units(steps.checked_mul(step)?);
            let candidate = Strategy::new(config, self.spec().clone(), *self.fees()).ok()?;
            let start = candidate.takeover_state(position).ok()?;
            let stress = candidate.stress_test(&start, mark_price, basis, tiers);
            Some(
                stress
                    .iter()
                    .find(|side| side.direction == direction)?
                    .margin_ratio
                    < limit,
            )
        };
        let mut low = div_ceil_i64(held, step);
        let high = side.config().max_position.units() / step;
        if low > high {
            return Suggestion::NoFeasiblePosition;
        }
        // 1. 最小的合法 `M`：现在这份配置本身是合法的，所以上界一定合法。
        let mut upper = high;
        while low < upper {
            let middle = low + (upper - low) / 2;
            if check(middle).is_some() {
                upper = middle;
            } else {
                low = middle + 1;
            }
        }
        match check(low) {
            Some(true) => {}
            Some(false) if low * step <= held => return Suggestion::HeldPositionTooLarge,
            Some(false) | None => return Suggestion::NoFeasiblePosition,
        }
        // 2. 合法区间里最大的达标 `M`。
        let mut high = high;
        while low < high {
            let middle = low + (high - low + 1) / 2;
            if check(middle) == Some(true) {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        Suggestion::Max(Quantity::from_units(low * step))
    }
}

/// 满仓保证金率超标时，建议把 `M` 改成多少。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suggestion {
    /// 改成这个数就达标。
    Max(Quantity),
    /// 只留标的上已经持有的仓位就已经超标：缩小还没成交的部分也没用。
    HeldPositionTooLarge,
    /// 在现在的批次和格子设置下，没有既合法（每格够最低下单金额）又达标的 `M`：要改范围、
    /// 间距或者主仓比例，或者增加资金。
    NoFeasiblePosition,
}

fn div_ceil_i64(numerator: i64, denominator: i64) -> i64 {
    numerator / denominator + i64::from(numerator % denominator > 0)
}

/// 一侧满仓：已经持有的部分和还没成交的部分，各自的数量和成本。
#[derive(Debug, Clone, Copy)]
struct FullPosition {
    direction: Direction,
    /// 已经持有的数量（接管的仓位），成本按当前标记价算。
    held: Quantity,
    held_cost: i128,
    /// 还没成交的数量，成本按挂单价算（1e-16）。
    pending: Quantity,
    pending_cost: i128,
    /// 还没成交那部分的建仓手续费（1e-16），按 taker 费率向上取整：和资金预检一样保守。
    pending_fee: i128,
}

impl FullPosition {
    /// 各批主仓按批次价、各格按建仓价全部成交；`start` 里已经持有的部分按 `mark_price`。
    fn of(side: &SidePlan, start: &StrategyState, mark_price: Price, fees: &Fees) -> Self {
        let side_state = start.side(side.direction());
        let mut held = 0_i64;
        let mut pending = 0_i64;
        let mut pending_cost = 0_i128;
        for batch in side.batches().batches() {
            let filled = side_state
                .and_then(|state| state.batch(batch.index))
                .map_or(Quantity::ZERO, |state| {
                    state.filled.min(batch.core_quantity)
                });
            let rest = batch.core_quantity.saturating_sub(filled);
            held += filled.units();
            pending += rest.units();
            pending_cost += Notional::of(rest, batch.price).raw();
        }
        for cell in side.grid().cells() {
            let filled = side_state
                .and_then(|state| state.cell(cell.index))
                .map_or(Quantity::ZERO, |state| state.held(cell.quantity));
            let rest = cell.quantity.saturating_sub(filled);
            held += filled.units();
            pending += rest.units();
            pending_cost += Notional::of(rest, cell.entry_price).raw();
        }
        let held = Quantity::from_units(held);
        Self {
            direction: side.direction(),
            held,
            held_cost: Notional::of(held, mark_price).raw(),
            pending: Quantity::from_units(pending),
            pending_cost,
            pending_fee: ceil_div(
                pending_cost * i128::from(fees.taker_rate.units()),
                i128::from(SCALE),
            ),
        }
    }

    fn quantity(&self) -> Quantity {
        self.held + self.pending
    }

    fn cost(&self) -> i128 {
        self.held_cost + self.pending_cost
    }

    fn average_entry(&self) -> Price {
        if self.quantity().is_zero() {
            return Price::ZERO;
        }
        Price::from_units((self.cost() / i128::from(self.quantity().units())) as i64)
    }

    /// 价格在 `price` 时这一侧的浮动盈亏，1e-16：做多 `M × 价格 − 成本`，做空反过来。
    fn pnl(&self, price: Price) -> i128 {
        let value = Notional::of(self.quantity(), price).raw();
        match self.direction {
            Direction::Long => value - self.cost(),
            Direction::Short => self.cost() - value,
        }
    }

    /// 账户权益减去全部维持保证金，1e-16：降到 0 就强平。
    fn margin_left(&self, basis: &AccountBasis, tiers: &MarginTiers, price: Price) -> i128 {
        self.equity(basis, price)
            - basis.other_maintenance.raw()
            - tiers
                .maintenance_margin(Notional::of(self.quantity(), price))
                .raw()
    }

    /// 账户比率的分母：余额 − 逐仓保证金 + 这一侧的浮动盈亏。
    /// 账户比率的分母：余额 − 逐仓保证金 + 这一侧的浮动盈亏 − 还没成交部分的建仓手续费。
    fn equity(&self, basis: &AccountBasis, price: Price) -> i128 {
        basis.balance.raw() - basis.isolated_margin.raw() + self.pnl(price) - self.pending_fee
    }

    fn margin_ratio(&self, basis: &AccountBasis, tiers: &MarginTiers, price: Price) -> Rate {
        let used = basis.other_maintenance.raw()
            + tiers
                .maintenance_margin(Notional::of(self.quantity(), price))
                .raw();
        let equity = self.equity(basis, price);
        if equity <= 0 {
            return Rate::from_units(RATIO_CAP_UNITS);
        }
        // 向上取整：比率宁可高估，贴着阈值的配置不会被放过去。
        let units =
            ceil_div(used.max(0) * i128::from(SCALE), equity).min(i128::from(RATIO_CAP_UNITS));
        Rate::from_units(units as i64)
    }

    /// 强平价：权益刚好等于维持保证金的价格。按价格二分（维持保证金分档，不好直接解）。
    ///
    /// 做多时价格越低剩得越少；做空时价格越高剩得越少。做多到价格为零还剩得下，就不会
    /// 强平；做空到能表示的最高价格还剩得下，也当作不会强平。
    fn liquidation(&self, basis: &AccountBasis, tiers: &MarginTiers) -> Liquidation {
        match self.liquidation_price(basis, tiers) {
            Ok(Some(price)) => Liquidation::At { price },
            Ok(None) => Liquidation::Never,
            Err(OutOfRange) => Liquidation::OutOfRange,
        }
    }

    /// `Ok(None)` 是不会强平，`Err` 是强平价超出能表示的范围。
    fn liquidation_price(
        &self,
        basis: &AccountBasis,
        tiers: &MarginTiers,
    ) -> Result<Option<Price>, OutOfRange> {
        if self.quantity().is_zero() {
            return Ok(None);
        }
        let left = |units: i64| self.margin_left(basis, tiers, Price::from_units(units));
        match self.direction {
            Direction::Long => {
                if left(0) >= 0 {
                    return Ok(None);
                }
                // left 随价格上升而增加：找最高的那个还不够的价格。
                let mut high = self.average_entry().units().max(1);
                while left(high) < 0 {
                    high = doubled(high).ok_or(OutOfRange)?;
                }
                let mut low = 0_i64;
                while high - low > 1 {
                    let middle = low + (high - low) / 2;
                    if left(middle) < 0 {
                        low = middle;
                    } else {
                        high = middle;
                    }
                }
                Ok(Some(Price::from_units(high)))
            }
            Direction::Short => {
                let mut low = self.average_entry().units().max(1);
                if left(low) < 0 {
                    // 均价上就已经不够：从零往上找。
                    low = 0;
                }
                // 价格越高剩得越少：上界一直加倍，直到夹住强平价。到了能表示的最高价格还剩得下，
                // 才算不会强平。
                let mut high = low.max(1);
                while left(high) >= 0 {
                    high = doubled(high).ok_or(OutOfRange)?;
                }
                while high - low > 1 {
                    let middle = low + (high - low) / 2;
                    if left(middle) >= 0 {
                        low = middle;
                    } else {
                        high = middle;
                    }
                }
                Ok(Some(Price::from_units(low)))
            }
        }
    }
}

/// 强平价超出能表示的价格范围。
struct OutOfRange;

/// 二分时把上界翻倍；翻倍溢出就用能表示的最高价格，那里也夹不住才返回 `None`。
fn doubled(high: i64) -> Option<i64> {
    match high.checked_mul(2) {
        Some(next) => Some(next),
        None if high < i64::MAX => Some(i64::MAX),
        None => None,
    }
}

/// 最不利的价格：做多是范围下沿，做空是范围上沿（第 7.3 节）。观点失效价属于后续任务。
fn adverse_price(side: &SidePlan) -> Price {
    match side.direction() {
        Direction::Long => side.config().price_low,
        Direction::Short => side.config().price_high,
    }
}

/// 1e-16 的整数换回金额（Notional 只能从 1e-8 构造），向上取整。
fn notional_ceil(raw: i128) -> Notional {
    Notional::from_scaled(ceil_div(raw.max(0), i128::from(SCALE)) as i64)
}

fn ceil_div(numerator: i128, denominator: i128) -> i128 {
    let quotient = numerator / denominator;
    if numerator % denominator > 0 {
        quotient + 1
    } else {
        quotient
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountBasis, MarginTier, MarginTiers, MarginTiersError};
    use crate::fixtures::{core_long_strategy, core_short_strategy, neutral_grid_strategy};
    use crate::side::Direction;
    use crate::units::{Notional, Position, Price, Quantity, Rate};

    fn notional(value: &str) -> Notional {
        Notional::parse(value).unwrap()
    }

    fn basis(balance: &str) -> AccountBasis {
        AccountBasis {
            balance: notional(balance),
            isolated_margin: Notional::ZERO,
            other_maintenance: Notional::ZERO,
        }
    }

    /// 官方例子的形状：0 起 40 倍、1 万起 25 倍。
    fn btc_tiers() -> MarginTiers {
        MarginTiers::new(vec![
            MarginTier {
                lower_bound: Notional::ZERO,
                max_leverage: 40,
            },
            MarginTier {
                lower_bound: notional("10000"),
                max_leverage: 25,
            },
        ])
        .unwrap()
    }

    #[test]
    fn the_maintenance_margin_follows_the_tiers() {
        let tiers = btc_tiers();

        // 第 0 档：8000 × 1.25% = 100。
        assert_eq!(tiers.maintenance_margin(notional("8000")), notional("100"));
        // 第 1 档：75000 × 2% − 扣减额 10000 × (2% − 1.25%) = 1500 − 75 = 1425。
        assert_eq!(
            tiers.maintenance_margin(notional("75000")),
            notional("1425")
        );
        // 正好在档位下限上连续：10000 × 1.25% = 125 = 10000 × 2% − 75。
        assert_eq!(tiers.maintenance_margin(notional("10000")), notional("125"));
    }

    #[test]
    fn bad_tier_tables_are_refused() {
        assert_eq!(
            MarginTiers::new(vec![MarginTier {
                lower_bound: notional("1"),
                max_leverage: 40
            }]),
            Err(MarginTiersError::BadBounds)
        );
        assert_eq!(
            MarginTiers::new(vec![MarginTier {
                lower_bound: Notional::ZERO,
                max_leverage: 0
            }]),
            Err(MarginTiersError::ZeroLeverage)
        );
    }

    /// 设计第 3.2 节的做多例子满仓：主仓 .175 × (95+90+85+80) 千 = 61250，格子 .015 ×
    /// (95+…+76) 千 = 25650，成本 86900，`M = 1`，均价 86900。价格到 75000：浮亏 11900，
    /// 维持保证金（只有一档 40 倍）937.5。
    #[test]
    fn a_full_long_position_at_the_low_edge() {
        let strategy = core_long_strategy();

        let stress = strategy.stress_test(
            &strategy.initial_state(),
            Price::parse("95500").unwrap(),
            &basis("100000"),
            &MarginTiers::single(40),
        );

        assert_eq!(stress.len(), 1);
        let long = stress[0];
        assert_eq!(long.direction, Direction::Long);
        assert_eq!(long.adverse_price, Price::parse("75000").unwrap());
        assert_eq!(long.position, Quantity::parse("1").unwrap());
        assert_eq!(long.average_entry, Price::parse("86900").unwrap());
        // 建仓手续费按 taker 0.045%：86900 × 0.00045 = 39.105。
        // 937.5 ÷ (100000 − 11900 − 39.105) = 0.0106460…，向上取整。
        assert_eq!(long.margin_ratio, Rate::parse("0.01064605").unwrap());
        // 资金足够：价格到零也不会强平。
        assert_eq!(long.liquidation, super::Liquidation::Never);
    }

    /// 余额 15000：权益 15000 + X − 86900 − 手续费 39.105 = 维持保证金 0.0125 X。
    #[test]
    fn the_long_liquidation_price_is_where_equity_meets_maintenance() {
        let strategy = core_long_strategy();

        let stress = strategy.stress_test(
            &strategy.initial_state(),
            Price::parse("95500").unwrap(),
            &basis("15000"),
            &MarginTiers::single(40),
        );

        let super::Liquidation::At { price: liquidation } = stress[0].liquidation else {
            panic!("expected a liquidation price")
        };
        // 再扣建仓手续费 39.105：X = (71900 + 39.105) ÷ 0.9875。
        assert!(
            (liquidation.units() - 7_284_972_658_228).abs() <= 1,
            "{liquidation}"
        );
        // 范围下沿 75000 时还没强平，保证金率在 100% 以下。
        assert!(stress[0].margin_ratio < Rate::parse("1").unwrap());
    }

    /// 做空镜像：价格到上沿 95000 最不利；强平价在上方。
    #[test]
    fn a_full_short_position_at_the_high_edge() {
        let strategy = core_short_strategy();

        let stress = strategy.stress_test(
            &strategy.initial_state(),
            Price::parse("95500").unwrap(),
            &basis("15000"),
            &MarginTiers::single(40),
        );

        let short = stress[0];
        assert_eq!(short.direction, Direction::Short);
        assert_eq!(short.adverse_price, Price::parse("95000").unwrap());
        let super::Liquidation::At { price: liquidation } = short.liquidation else {
            panic!("expected a liquidation price")
        };
        assert!(liquidation > short.average_entry, "{liquidation}");
        // 强平价上权益刚好用完维持保证金。
        let at = |price: Price| {
            let pnl = (short.average_entry.units() - price.units()) as i128;
            // 维持保证金按实现的取整方向：往大里取。
            // 建仓手续费：均价 × 1 × 0.045%，往大里取。
            let fee = super::ceil_div(short.average_entry.units() as i128 * 45_000, 100_000_000);
            15_000 * 100_000_000_i128 + pnl - fee - (price.units() as i128 + 79) / 80
        };
        assert!(at(liquidation) >= 0);
        assert!(at(Price::from_units(liquidation.units() + 1)) < 0);
    }

    /// 中性网格两侧分开算，不相互抵消。
    #[test]
    fn a_neutral_grid_stresses_each_side_on_its_own() {
        let strategy = neutral_grid_strategy();

        let stress = strategy.stress_test(
            &strategy.initial_state(),
            Price::parse("95500").unwrap(),
            &basis("1000"),
            &MarginTiers::single(40),
        );

        assert_eq!(stress.len(), 2);
        assert_eq!(stress[0].direction, Direction::Long);
        assert_eq!(stress[0].adverse_price, Price::parse("79").unwrap());
        assert_eq!(stress[0].position, Quantity::parse("1").unwrap());
        assert_eq!(stress[1].direction, Direction::Short);
        assert_eq!(stress[1].adverse_price, Price::parse("96").unwrap());
        assert_eq!(stress[1].position, Quantity::parse("0.25").unwrap());
    }

    /// 建议的最大仓位：照着它改 `M`、重新划分之后，满仓保证金率确实低于给定比率；
    /// 再多一个数量单位就不低于。
    #[test]
    fn the_suggested_position_holds_after_repartitioning() {
        let strategy = core_long_strategy();
        let basis = basis("15000");
        let tiers = MarginTiers::single(40);
        let limit = Rate::parse("0.3").unwrap();
        let mark = Price::parse("95500").unwrap();

        let super::Suggestion::Max(suggested) = strategy.suggest_max_position(
            Direction::Long,
            Position::ZERO,
            mark,
            &basis,
            &tiers,
            limit,
        ) else {
            panic!("expected a suggestion");
        };

        assert!(suggested < Quantity::parse("1").unwrap());
        let ratio_with = |max_position: Quantity| {
            let mut config = strategy.config().clone();
            config.long.as_mut().unwrap().max_position = max_position;
            let candidate =
                crate::Strategy::new(config, strategy.spec().clone(), *strategy.fees()).unwrap();
            candidate.stress_test(&candidate.initial_state(), mark, &basis, &tiers)[0].margin_ratio
        };
        assert!(ratio_with(suggested) < limit);
        let one_more = Quantity::from_units(suggested.units() + 100_000);
        assert!(ratio_with(one_more) >= limit);
    }

    /// 接管的仓位按当前标记价计成本：已有 1 多单（满仓就是它），标记价 95500，均价按
    /// 95500 算而不是计划的 86900；跌到 75000 浮亏 20500，余额 14500 扛不住。
    #[test]
    fn a_taken_over_position_is_priced_at_the_mark() {
        let strategy = core_long_strategy();
        let start = strategy
            .takeover_state(crate::units::Position::parse("1").unwrap())
            .unwrap();

        let stress = strategy.stress_test(
            &start,
            Price::parse("95500").unwrap(),
            &basis("14500"),
            &MarginTiers::single(40),
        );

        assert_eq!(stress[0].average_entry, Price::parse("95500").unwrap());
        assert_eq!(stress[0].margin_ratio, Rate::from_units(100 * 100_000_000));
        assert!(matches!(
            stress[0].liquidation,
            super::Liquidation::At { price } if price > Price::parse("75000").unwrap()
        ));
    }

    /// 除不尽时维持保证金往大里取：75001 ÷ 6 = 12500.1666…，报 12500.16666667。
    #[test]
    fn the_maintenance_margin_rounds_up() {
        let tiers = MarginTiers::single(3);

        assert_eq!(
            tiers.maintenance_margin(notional("75001")),
            notional("12500.16666667")
        );
    }

    /// 做空的强平价远在均价之上也找得到：均价 100、余额 200000，强平价约 197629.6。
    #[test]
    fn a_short_liquidation_far_above_the_entry_is_found() {
        use crate::config::{SideConfig, StrategyConfig};
        use crate::fixtures::{btc_fees, cheap_spec};
        use crate::instrument::Instrument;
        let config = StrategyConfig {
            instrument: Instrument::try_new("BTC").unwrap(),
            long: None,
            short: Some(SideConfig {
                price_low: Price::parse("99").unwrap(),
                price_high: Price::parse("101").unwrap(),
                max_position: Quantity::parse("1").unwrap(),
                core_ratio: Rate::parse("1").unwrap(),
                batch_count: Some(1),
                grid_spacing: Price::parse("1").unwrap(),
                grid_spread: Price::parse("1").unwrap(),
                throttle_interval: None,
            }),
        };
        let strategy = crate::Strategy::new(config, cheap_spec(), btc_fees()).unwrap();

        let stress = strategy.stress_test(
            &strategy.initial_state(),
            Price::parse("100").unwrap(),
            &basis("200000"),
            &MarginTiers::single(40),
        );

        let super::Liquidation::At { price: liquidation } = stress[0].liquidation else {
            panic!("expected a liquidation price")
        };
        assert!(
            liquidation > Price::parse("190000").unwrap()
                && liquidation < Price::parse("200000").unwrap(),
            "{liquidation}"
        );
    }

    /// 已经持有的仓位本身就扛不住：给不出「阈值以内的最大仓位」。
    #[test]
    fn no_suggestion_when_the_held_position_alone_is_too_much() {
        let strategy = core_long_strategy();

        let suggested = strategy.suggest_max_position(
            Direction::Long,
            Position::parse("1").unwrap(),
            Price::parse("95500").unwrap(),
            &basis("14500"),
            &MarginTiers::single(40),
            Rate::parse("0.3").unwrap(),
        );

        assert_eq!(suggested, super::Suggestion::HeldPositionTooLarge);
    }

    /// 没有持仓、资金只有 1 美元：合法的配置都超标，更小的又不够最低下单金额——说「没有
    /// 可行的仓位」，不说「已有仓位超标」。
    #[test]
    fn no_feasible_position_is_not_blamed_on_a_held_position() {
        let strategy = core_long_strategy();

        let suggested = strategy.suggest_max_position(
            Direction::Long,
            Position::ZERO,
            Price::parse("95500").unwrap(),
            &basis("1"),
            &MarginTiers::single(40),
            Rate::parse("0.3").unwrap(),
        );

        assert_eq!(suggested, super::Suggestion::NoFeasiblePosition);
    }

    /// 上界翻倍溢出时用能表示的最高价格接着夹：强平价 800 亿也找得到。
    #[test]
    fn a_liquidation_price_near_the_top_of_the_range_is_found() {
        assert_eq!(super::doubled(2), Some(4));
        assert_eq!(super::doubled(i64::MAX / 2 + 1), Some(i64::MAX));
        assert_eq!(super::doubled(i64::MAX), None);
    }

    /// 搜索次数有界：`M = 100000`、数量精度 .00001 的配置，合法的仓位全都超标，也很快
    /// 给出「没有可行的仓位」，不会一个数量单位一个数量单位地往下退。
    #[test]
    fn the_suggestion_search_stays_short_for_a_huge_position() {
        use crate::config::{SideConfig, StrategyConfig};
        use crate::fixtures::{btc_fees, fine_spec};
        use crate::instrument::Instrument;
        let config = StrategyConfig {
            instrument: Instrument::try_new("BTC").unwrap(),
            long: Some(SideConfig {
                price_low: Price::parse("75000").unwrap(),
                price_high: Price::parse("95000").unwrap(),
                max_position: Quantity::parse("100000").unwrap(),
                core_ratio: Rate::parse("0.00000001").unwrap(),
                batch_count: Some(1),
                grid_spacing: Price::parse("1000").unwrap(),
                grid_spread: Price::parse("1000").unwrap(),
                throttle_interval: None,
            }),
            short: None,
        };
        let strategy = crate::Strategy::new(config, fine_spec(), btc_fees()).unwrap();
        let started = std::time::Instant::now();

        let suggested = strategy.suggest_max_position(
            Direction::Long,
            Position::ZERO,
            Price::parse("95500").unwrap(),
            &basis("1"),
            &MarginTiers::single(40),
            Rate::parse("0.3").unwrap(),
        );

        assert_eq!(suggested, super::Suggestion::NoFeasiblePosition);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// 还没成交部分的建仓手续费也要扣：余额 13780 时不扣是 49.87%，扣掉 39.105 之后
    /// 超过 50%。
    #[test]
    fn the_entry_fees_of_the_pending_part_count() {
        let strategy = core_long_strategy();

        let stress = strategy.stress_test(
            &strategy.initial_state(),
            Price::parse("95500").unwrap(),
            &basis("13780"),
            &MarginTiers::single(40),
        );

        assert!(stress[0].margin_ratio >= Rate::parse("0.5").unwrap());
    }
}
