//! 限速（第 4.1 节）：同一侧在 τ 时间内最多启动一个单位。
//!
//! **单位**：`r > 0` 时是一批主仓，`r = 0` 时是一个格子。启动顺序从靠近空隙的一端
//! 开始，也就是单位下标从小到大。做多网格只有一侧，所以只有一份放行进度。
//!
//! **规则**：一个单位的第一笔成交之后满 τ，才放行下一个单位；同一时间最多一个单位
//! 在等着启动。已启动单位的后续成交、格子的买卖循环都不受限速约束。
//!
//! 放行只决定「哪些单位可以挂单」，不撤销任何在场订单、也不释放任何预留。启动时没放行
//! 的单位本来就没有在场订单；运行中从不限速改成限速时，最后一个已经开始的单位之前的
//! 单位全部保持放行（[`Strategy::rebuild_throttle`]），它们在场上的订单留在原地，
//! 等重新放行后再由规划处理。
//!
//! **两个保守之处**（设计没有写死，按「宁可少建仓」取舍）：
//! - 靠前的单位一直没有成交时，后面的单位永远不放开，不因为等待时间长就跳过它；
//! - 时间都取自实际成交时刻，重启或暂停都不会多给额度。

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::Strategy;
use crate::order::Unit;
use crate::state::StrategyState;

/// 限速的放行进度（第 12 节要求持久化）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ThrottleState {
    /// 已经按顺序放行的单位数：下标小于它的单位可以挂单。
    ///
    /// 不限速时等于单位总数（全部可以启动）。
    pub released: usize,
    /// 最近一次放行的时刻。
    ///
    /// 它只作放行时刻的下限：判断下一个单位能不能放行时，取它和「最近放行单位的第一笔
    /// 成交时刻」里更晚的那个。这样即使成交时刻早于放行时刻（时钟回拨、数据迟到），
    /// 也不会提前放行。
    pub released_at: Option<DateTime<Utc>>,
    /// 最近放行的那个单位的第一笔成交时刻；它还没有成交时是 `None`。
    ///
    /// 它是下一个单位放行时刻的起点。已在放行的单位之后再有成交不会改写它：那些成交
    /// 属于单位内部，不重新计时。
    pub first_fill_at: Option<DateTime<Utc>>,
}

impl Strategy {
    /// 本侧的单位数：`r > 0` 时是批数，`r = 0` 时是格数。
    pub fn unit_count(&self) -> usize {
        if self.config().long.core_ratio.is_zero() {
            self.grid().len()
        } else {
            self.batches().len()
        }
    }

    /// 限速间隔 τ；`None` 表示不限速。
    pub fn throttle_interval(&self) -> Option<Duration> {
        self.config().long.throttle_interval
    }

    /// 这个单位是否受本侧限速约束。
    ///
    /// 单位就是受约束的那些：`r > 0` 时批次是单位，格子跟随所属批次放行；`r = 0` 时
    /// 格子自己就是单位。
    fn throttled(&self, unit: Unit) -> bool {
        let batches_are_units = !self.config().long.core_ratio.is_zero();
        match unit {
            Unit::Batch(_) => batches_are_units,
            Unit::Cell(_) => !batches_are_units,
        }
    }

    /// 这个单位已经放行了没有。不受限速约束的单位（跟随批次的格子）总是可以挂单。
    pub fn is_released(&self, state: &StrategyState, unit: Unit) -> bool {
        if !self.throttled(unit) {
            return true;
        }
        unit_index(unit) < state.throttle().released
    }

    /// 按限速放行下一个单位，返回状态是否有变化。
    ///
    /// 纯计算：时间由调用方传入，成交时刻由 [`Strategy::apply_fill`] 记下来。
    /// 调用方在记账之后、规划之前调用它，新放行的单位这一步就会挂上单。
    pub fn advance_throttle(&self, state: &mut StrategyState, now: DateTime<Utc>) -> bool {
        let units = self.unit_count();
        let Some(interval) = self.throttle_interval() else {
            // 不限速：全部单位一开始就可以启动。
            if state.throttle().released == units {
                return false;
            }
            let throttle = state.throttle_mut();
            throttle.released = units;
            return true;
        };

        let mut changed = false;
        loop {
            let throttle = state.throttle_mut();
            if throttle.released >= units {
                return changed;
            }
            if throttle.released == 0 {
                // 第一个单位立刻放行：限速限制的是「多久之后启动下一个」。
                throttle.released = 1;
                throttle.released_at = Some(now);
                throttle.first_fill_at = None;
                changed = true;
                continue;
            }
            // 第一个单位在启动时就放行了，那时还没有时钟：这里补记放行时刻。
            // 它只是放行时刻的下限，所以第一次调用之后就不再改动。
            if throttle.released_at.is_none() {
                throttle.released_at = Some(now);
                changed = true;
            }
            // 靠前的单位还没有第一笔成交：后面的单位不放开，不追高也不跳过。
            let Some(first_fill_at) = throttle.first_fill_at else {
                return changed;
            };
            let released_at = throttle.released_at.unwrap_or(first_fill_at);
            // 间隔大到算不出放行时刻时（配置里给了个极大的 τ）当作不放行：这里不能
            // panic，否则每一步都会崩，而且崩在 runtime 里看不出是哪个参数的问题。
            let Some(ready_at) = first_fill_at.max(released_at).checked_add_signed(interval) else {
                return changed;
            };
            if now < ready_at {
                return changed;
            }
            throttle.released += 1;
            throttle.released_at = Some(now);
            // 新放行的单位还没成交：下一个单位的放行时刻从它的第一笔成交算起。
            throttle.first_fill_at = None;
            changed = true;
        }
    }

    /// 按 `now` 这一刻重建放行进度，用于运行中改限速参数（第 4.1 节）。
    ///
    /// 改成限速时，**已经开始的单位保持放行**，只有还没开始的单位受限速约束：它们里的
    /// 第一个在 `now + τ` 放行，之后按正常规则依次放行。已经开始的单位不会再产生「第一笔
    /// 成交」（尤其已经结束的批次），把它们退回未放行会让后面的单位永远等下去。
    ///
    /// 重建之后 `released_at` 和 `first_fill_at` 都取 `now`，于是下一个单位的放行时刻是
    /// `now + τ`，与「最后一个已开始的单位在 `now` 成交」等价。不限速时全部放行。
    ///
    /// 只改放行进度，不动任何预留，也不撤任何在场订单：回到未放行的单位在场上的订单
    /// 留在原地，等重新放行后再由规划处理。
    pub fn rebuild_throttle(&self, state: &mut StrategyState, now: DateTime<Utc>) {
        let released = match self.throttle_interval() {
            Some(_) => self.started_through(state).max(1).min(self.unit_count()),
            None => self.unit_count(),
        };
        state.set_throttle(ThrottleState {
            released,
            released_at: Some(now),
            first_fill_at: Some(now),
        });
    }

    /// 已经开始的单位所覆盖的放行范围：最后一个「已经开始」的单位的下标 + 1，
    /// 一个都没开始时是 0。
    ///
    /// - 一批主仓「已经开始」指它有过成交，或者已经结束；
    /// - 一个格子「已经开始」指它已经买回了一部分（还在持有）。
    ///
    /// 取最后一个而不是数连续前缀：放行顺序不保证成交连续。不限速时靠前的批次可能被拒
    /// 或一直没到价，靠后的批次却已经成交结束；只数前缀会把它算作没开始，退回未放行之后
    /// 它又不会再产生「第一笔成交」，后面的单位就永远等不到放行。中间没开始的单位跟着
    /// 一起放行，照常挂单，不会永久等待。
    ///
    /// 格子把买回的仓位全部卖光之后，状态与没动过的格子一样，这里会把它算作没开始。
    /// 它只是让修改之后的放行偏保守：重新放行之后照常挂单，不会卡住。
    fn started_through(&self, state: &StrategyState) -> usize {
        let mut through = 0;
        if self.config().long.core_ratio.is_zero() {
            for cell in self.grid().cells() {
                let Some(cell_state) = state.cell(cell.index) else {
                    break;
                };
                if cell_state.to_buy_back < cell.quantity {
                    through = cell.index.0 + 1;
                }
            }
        } else {
            for (index, batch_state) in state.batches().iter().enumerate() {
                if batch_state.finished || !batch_state.filled.is_zero() {
                    through = index + 1;
                }
            }
        }
        through
    }

    /// 记下一笔成交的时刻，看看它是不是「最近放行单位的第一笔成交」，返回是否有变化。
    ///
    /// 只有正在等待成交的那个单位算数：已放行单位的后续成交、更早单位的迟到成交，
    /// 都不重新计时（不然每来一笔成交就能把下一个单位的放行推后或提前）。
    ///
    /// 同一个单位可能被记两次：下单即成交时交易所的回执没有成交时间戳，只能用收到
    /// 回执的时刻作**保守上界**先记下来；真实流水到了之后用它校正。所以这里取更早的
    /// 那个——放行时刻从第一笔**实际成交**算起，记晚了会让限速偏松（第 4.1 节）。
    pub fn record_fill_time(
        &self,
        state: &mut StrategyState,
        unit: Unit,
        filled_at: DateTime<Utc>,
    ) -> bool {
        if !self.throttled(unit) {
            return false;
        }
        let throttle = state.throttle_mut();
        if throttle.released == 0 {
            return false;
        }
        // 单位按下标顺序放行，所以最近放行的那个单位的下标是 `released - 1`。
        if unit_index(unit) + 1 != throttle.released {
            return false;
        }
        match throttle.first_fill_at {
            // 已经记过一笔更早或同样的成交：不改写。
            Some(existing) if existing <= filled_at => false,
            _ => {
                throttle.first_fill_at = Some(filled_at);
                true
            }
        }
    }
}

/// 单位在启动顺序上的下标。批次的顺序就是批次下标，格子的顺序就是格子下标
/// （两者都是从靠近空隙的一端开始）。
fn unit_index(unit: Unit) -> usize {
    match unit {
        Unit::Batch(index) => index.0,
        Unit::Cell(index) => index.0,
    }
}

#[cfg(test)]
mod tests {
    use super::ThrottleState;
    use crate::Strategy;
    use crate::batch::BatchIndex;
    use crate::fixtures::{
        at, core_long_strategy, long_grid_strategy, throttled_core_long_strategy,
        throttled_core_long_with_interval, throttled_long_grid_strategy,
    };
    use crate::grid::CellIndex;
    use crate::order::{ClientOrderId, OrderPurpose, OrderRecord, OrderStatus, Unit};
    use crate::planning::WaitReason;
    use crate::state::StrategyState;
    use crate::units::{Price, Quantity};
    use chrono::{DateTime, Duration, Utc};

    /// 记一笔成交：每笔成交都是一张新订单，所以累计量就是这一笔的数量。
    fn fill(
        strategy: &Strategy,
        state: &mut StrategyState,
        unit: Unit,
        purpose: OrderPurpose,
        quantity: &str,
        filled_at: DateTime<Utc>,
    ) {
        let mut order = OrderRecord {
            client_order_id: ClientOrderId::try_new("0x01").unwrap(),
            unit,
            purpose,
            limit_price: Price::parse("95000").unwrap(),
            quantity: Quantity::parse(quantity).unwrap(),
            cumulative_filled: Quantity::ZERO,
            reduce_only: purpose == OrderPurpose::GridSell,
            status: OrderStatus::Open,
        };
        strategy.apply_fill(
            state,
            &mut order,
            Quantity::parse(quantity).unwrap(),
            filled_at,
        );
    }

    fn core_fill(
        strategy: &Strategy,
        state: &mut StrategyState,
        batch: usize,
        quantity: &str,
        filled_at: DateTime<Utc>,
    ) {
        fill(
            strategy,
            state,
            Unit::Batch(BatchIndex(batch)),
            OrderPurpose::CoreBuy,
            quantity,
            filled_at,
        );
    }

    #[test]
    fn b02_only_one_unit_runs_until_the_previous_one_fills_and_the_interval_passes() {
        // τ = 1 小时。
        let strategy = throttled_core_long_strategy();
        let mut state = strategy.initial_state();

        // 刚启动：只有第一个单位放行，还没有放行时刻（启动时才知道）。
        assert_eq!(state.throttle().released, 1);
        assert_eq!(state.throttle().released_at, None);
        assert!(strategy.advance_throttle(&mut state, at(0)));
        assert_eq!(state.throttle().released, 1);
        assert_eq!(state.throttle().released_at, Some(at(0)));

        // 只过去不到 τ，而且还没有成交：不放行第二个单位。
        assert!(!strategy.advance_throttle(&mut state, at(3599)));
        assert_eq!(state.throttle().released, 1);

        // 第 0 批主仓在 t = 10 成交 .02：放行时刻从这一笔算起，不是从启动时刻算起。
        core_fill(&strategy, &mut state, 0, "0.02", at(10));
        assert_eq!(state.throttle().first_fill_at, Some(at(10)));
        assert!(!strategy.advance_throttle(&mut state, at(3609)));
        assert_eq!(state.throttle().released, 1, "满 τ 之前不放行下一个单位");

        assert!(strategy.advance_throttle(&mut state, at(3610)));
        assert_eq!(state.throttle().released, 2);
        assert_eq!(state.throttle().released_at, Some(at(3610)));
        assert_eq!(state.throttle().first_fill_at, None);

        // 第 1 批还没成交：再久也不放行第 2 批。
        assert!(!strategy.advance_throttle(&mut state, at(1_000_000)));
        assert_eq!(state.throttle().released, 2);
    }

    #[test]
    fn b03_a_unit_that_never_fills_keeps_the_next_ones_waiting() {
        let strategy = throttled_core_long_strategy();
        let mut state = strategy.initial_state();
        strategy.advance_throttle(&mut state, at(0));
        assert!(state.throttle().first_fill_at.is_none());

        // 价格一直没到，主仓始终没有成交。
        for seconds in [1, 3600, 100_000] {
            assert!(!strategy.advance_throttle(&mut state, at(seconds)));
        }
        assert_eq!(state.throttle().released, 1);

        // 规划里只有第 0 批的主仓买单，而且报出「等待限速放行」。
        let plan = strategy.plan(&state, &[], &[], Price::parse("80000").unwrap());

        assert_eq!(plan.place.len(), 1);
        assert_eq!(plan.place[0].unit, Unit::Batch(BatchIndex(0)));
        assert_eq!(plan.place[0].limit_price, Price::parse("95000").unwrap());
        assert!(plan.waiting.contains(&WaitReason::WaitingForThrottle));
    }

    #[test]
    fn b02_later_fills_of_an_earlier_unit_do_not_restart_the_interval() {
        let strategy = throttled_core_long_strategy();
        let mut state = strategy.initial_state();
        strategy.advance_throttle(&mut state, at(0));
        core_fill(&strategy, &mut state, 0, "0.02", at(10));
        strategy.advance_throttle(&mut state, at(3610));
        assert_eq!(state.throttle().released, 2);

        // 第 0 批又来了一笔成交：它是已放行单位的后续成交，不是第 1 批的第一笔成交。
        core_fill(&strategy, &mut state, 0, "0.03", at(3620));

        assert_eq!(state.throttle().first_fill_at, None);
        assert!(!strategy.advance_throttle(&mut state, at(100_000)));
        assert_eq!(
            state.throttle().released,
            2,
            "第 1 批没有成交，第 2 批就不放开"
        );

        // 第 1 批成交之后，下一批从它的成交时刻算起。
        core_fill(&strategy, &mut state, 1, "0.02", at(200_000));
        assert_eq!(state.throttle().first_fill_at, Some(at(200_000)));
        assert!(!strategy.advance_throttle(&mut state, at(203_599)));
        assert!(strategy.advance_throttle(&mut state, at(203_600)));
        assert_eq!(state.throttle().released, 3);
    }

    #[test]
    fn b02_cells_of_a_released_batch_are_not_throttled() {
        let strategy = throttled_core_long_strategy();
        let mut state = strategy.initial_state();
        strategy.advance_throttle(&mut state, at(0));

        // 第 0 批主仓买完：本批五格启用，它们照常挂买单，不受限速约束。
        core_fill(&strategy, &mut state, 0, "0.175", at(10));
        strategy.advance(&mut state, &[]);
        assert!(state.cells()[..5].iter().all(|cell| cell.enabled));

        let plan = strategy.plan(&state, &[], &[], Price::parse("95000").unwrap());

        for index in 0..5 {
            assert!(
                plan.place.iter().any(|intent| {
                    intent.unit == Unit::Cell(CellIndex(index))
                        && intent.purpose == OrderPurpose::GridBuy
                }),
                "cell {index} must place its buy order"
            );
        }
        // 第 1 批还没放行：它没有主仓买单，等待原因里有「等待限速放行」。
        assert!(
            !plan
                .place
                .iter()
                .any(|intent| intent.unit == Unit::Batch(BatchIndex(1)))
        );
        assert!(plan.waiting.contains(&WaitReason::WaitingForThrottle));

        // 格子卖光之后照常买回：格子的循环不限速。
        for index in 0..5 {
            fill(
                &strategy,
                &mut state,
                Unit::Cell(CellIndex(index)),
                OrderPurpose::GridSell,
                "0.008",
                at(11),
            );
        }
        let plan = strategy.plan(&state, &[], &[], Price::parse("96000").unwrap());
        assert!(plan.place.iter().any(|intent| {
            intent.unit == Unit::Cell(CellIndex(0)) && intent.purpose == OrderPurpose::GridBuy
        }));
    }

    #[test]
    fn b24_without_a_core_the_units_are_the_cells() {
        let strategy = throttled_long_grid_strategy();
        let mut state = strategy.initial_state();
        assert_eq!(strategy.unit_count(), 20);
        assert_eq!(state.throttle().released, 1);
        strategy.advance_throttle(&mut state, at(0));

        // 只有最高的那一格（95000）放行。
        let plan = strategy.plan(&state, &[], &[], Price::parse("95500").unwrap());
        assert_eq!(plan.place.len(), 1);
        assert_eq!(plan.place[0].unit, Unit::Cell(CellIndex(0)));
        assert!(plan.waiting.contains(&WaitReason::WaitingForThrottle));

        // 第 0 格买回成交之后满 τ，才放行第 1 格。
        fill(
            &strategy,
            &mut state,
            Unit::Cell(CellIndex(0)),
            OrderPurpose::GridBuy,
            "0.05",
            at(100),
        );
        assert_eq!(state.throttle().first_fill_at, Some(at(100)));
        strategy.advance_throttle(&mut state, at(3700));
        assert_eq!(state.throttle().released, 2);

        let plan = strategy.plan(&state, &[], &[], Price::parse("95500").unwrap());
        assert!(plan.place.iter().any(|intent| {
            intent.unit == Unit::Cell(CellIndex(1)) && intent.purpose == OrderPurpose::GridBuy
        }));
        // 再往下的格子仍然等第 1 格成交。
        assert!(!plan.place.iter().any(|intent| {
            intent.unit == Unit::Cell(CellIndex(2)) && intent.purpose == OrderPurpose::GridBuy
        }));
    }

    #[test]
    fn b44_the_release_time_follows_the_configured_interval() {
        for (interval, released_at) in [
            (Duration::minutes(60), at(3600)),
            (Duration::minutes(1), at(60)),
            (Duration::zero(), at(0)),
        ] {
            let strategy = throttled_core_long_with_interval(interval);
            let mut state = strategy.initial_state();
            strategy.advance_throttle(&mut state, at(0));
            core_fill(&strategy, &mut state, 0, "0.02", at(0));

            assert!(
                strategy.advance_throttle(&mut state, released_at),
                "interval {interval:?} must release the next unit at {released_at}"
            );
            assert_eq!(state.throttle().released, 2);
        }
    }

    #[test]
    fn an_interval_too_large_to_add_never_releases_and_never_panics() {
        // 配置里可以给出一个极大的 τ：算不出放行时刻时当作不放行，不能 panic。
        let strategy = throttled_core_long_with_interval(Duration::MAX);
        let mut state = strategy.initial_state();
        strategy.advance_throttle(&mut state, at(0));
        core_fill(&strategy, &mut state, 0, "0.02", at(10));

        assert!(!strategy.advance_throttle(&mut state, at(10)));
        assert!(!strategy.advance_throttle(&mut state, at(1_000_000)));
        assert_eq!(state.throttle().released, 1);
    }

    #[test]
    fn without_a_throttle_every_unit_starts_at_once() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();

        assert_eq!(strategy.throttle_interval(), None);
        assert_eq!(strategy.unit_count(), 4);
        assert_eq!(state.throttle().released, 4);
        // 没有限速就没有可放行的：再推进也没有变化。
        assert!(!strategy.advance_throttle(&mut state, at(0)));

        let plan = strategy.plan(&state, &[], &[], Price::parse("96000").unwrap());
        for index in 0..4 {
            assert!(
                plan.place
                    .iter()
                    .any(|intent| intent.unit == Unit::Batch(BatchIndex(index))),
                "batch {index} must place its buy order"
            );
        }
        assert!(!plan.waiting.contains(&WaitReason::WaitingForThrottle));
    }

    /// 首笔成交时刻取真实成交时刻：先记下的估计值可以被更早的真实成交改回来，
    /// 更晚的成交不会把放行推后。
    #[test]
    fn an_earlier_fill_time_corrects_a_later_estimate() {
        let strategy = throttled_core_long_strategy();
        let mut state = strategy.initial_state();
        strategy.advance_throttle(&mut state, at(0));

        // 下单即成交时交易所的回执没有时间戳：先用收到回执的时刻记下来。
        assert!(strategy.record_fill_time(&mut state, Unit::Batch(BatchIndex(0)), at(100)));
        assert_eq!(state.throttle().first_fill_at, Some(at(100)));

        // 真实流水记的是 at(10)：校正到实际成交时刻。
        assert!(strategy.record_fill_time(&mut state, Unit::Batch(BatchIndex(0)), at(10)));
        assert_eq!(state.throttle().first_fill_at, Some(at(10)));

        // 第一个单位的后续成交不重新计时。
        assert!(!strategy.record_fill_time(&mut state, Unit::Batch(BatchIndex(0)), at(200)));
        assert_eq!(state.throttle().first_fill_at, Some(at(10)));

        // 放行时刻从真实成交算起。
        assert!(!strategy.advance_throttle(&mut state, at(3609)));
        assert!(strategy.advance_throttle(&mut state, at(3610)));
        assert_eq!(state.throttle().released, 2);
    }

    /// 只有「最近放行单位」的成交算数；还没放行的单位、还没启动的策略都不记。
    #[test]
    fn a_fill_of_another_unit_never_starts_the_interval() {
        let strategy = throttled_core_long_strategy();
        let mut state = strategy.initial_state();
        strategy.advance_throttle(&mut state, at(0));
        strategy.record_fill_time(&mut state, Unit::Batch(BatchIndex(0)), at(100));

        // 第 1 批还没放行。
        assert!(!strategy.record_fill_time(&mut state, Unit::Batch(BatchIndex(1)), at(10)));
        assert_eq!(state.throttle().first_fill_at, Some(at(100)));

        // 还没启动（放行数为 0）时也没有可记的。
        state.set_throttle(ThrottleState::default());
        assert!(!strategy.record_fill_time(&mut state, Unit::Batch(BatchIndex(0)), at(10)));
        assert_eq!(state.throttle().first_fill_at, None);
    }

    /// 运行中从不限速改成限速，还没有单位开始时：限速从修改时刻开始计时，不从零开始。
    #[test]
    fn turning_on_a_throttle_starts_the_interval_at_the_moment_of_the_change() {
        let mut state = core_long_strategy().initial_state();
        assert_eq!(state.throttle().released, 4, "不限速时全部单位都已放行");

        let throttled = throttled_core_long_strategy();
        throttled.rebuild_throttle(&mut state, at(100));

        assert_eq!(state.throttle().released, 1);
        assert_eq!(state.throttle().released_at, Some(at(100)));
        assert_eq!(state.throttle().first_fill_at, Some(at(100)));

        // 满 τ 就放行第二个单位，不需要第一个单位先成交。
        assert!(!throttled.advance_throttle(&mut state, at(3699)));
        assert_eq!(state.throttle().released, 1);
        assert!(throttled.advance_throttle(&mut state, at(3700)));
        assert_eq!(state.throttle().released, 2);
        assert_eq!(
            state.throttle().first_fill_at,
            None,
            "放行之后从下一个单位重新计时"
        );
    }

    /// 运行中改成限速：已经开始的单位保持放行，不退回未放行。
    ///
    /// 退回未放行会让后面的单位等一个永远不来的「第一笔成交」：已经结束的批次不会再成交。
    #[test]
    fn turning_on_a_throttle_keeps_the_units_that_already_started_released() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();
        // 第 0 批成交了一部分，第 1 批成交结束。
        core_fill(&strategy, &mut state, 0, "0.05", at(1));
        core_fill(&strategy, &mut state, 1, "0.175", at(2));
        strategy.advance(&mut state, &[]);
        assert!(state.batch(BatchIndex(1)).unwrap().finished);

        let throttled = throttled_core_long_strategy();
        throttled.rebuild_throttle(&mut state, at(100));

        assert_eq!(state.throttle().released, 2, "已经开始的两批保持放行");
        assert!(throttled.is_released(&state, Unit::Batch(BatchIndex(1))));
        // 还没开始的第 2 批在修改时刻满 τ 之后放行，不必等第 1 批再成交。
        assert!(!throttled.advance_throttle(&mut state, at(3699)));
        assert!(throttled.advance_throttle(&mut state, at(3700)));
        assert_eq!(state.throttle().released, 3);
    }

    /// 运行中改成限速：中间没开始的单位不挡住后面已经开始的单位。
    ///
    /// 不限速时靠前的批次可能被拒或一直没到价，靠后的批次却已经成交结束。只把「连续开始的
    /// 前缀」算作已开始的话，这个已结束的批次会被退回未放行：它不会再产生第一笔成交，满 τ
    /// 之后放行的就是它自己，放行进度停在那里，后面的批次永远等不到。
    #[test]
    fn a_gap_of_unstarted_units_does_not_strand_the_units_behind_it() {
        let strategy = core_long_strategy();
        let mut state = strategy.initial_state();
        // 第 0 批的主仓买单被拒：没有成交，也没有结束。第 1 批成交结束。
        core_fill(&strategy, &mut state, 1, "0.175", at(2));
        strategy.advance(&mut state, &[]);
        assert!(state.batch(BatchIndex(0)).unwrap().filled.is_zero());
        assert!(state.batch(BatchIndex(1)).unwrap().finished);

        let throttled = throttled_core_long_strategy();
        throttled.rebuild_throttle(&mut state, at(100));

        assert_eq!(state.throttle().released, 2, "放行到最后一个已开始的单位");
        assert_eq!(
            state.throttle().first_fill_at,
            Some(at(100)),
            "间隔的起点留着，不被已经开始的单位吃掉"
        );

        // 中间没开始的第 0 批也在放行范围里，照常挂单。
        let plan = throttled.plan(&state, &[], &[], Price::parse("96000").unwrap());
        assert!(
            plan.place
                .iter()
                .any(|intent| intent.unit == Unit::Batch(BatchIndex(0))),
            "没开始但在放行范围内的批次照常挂单"
        );

        // 满 τ 放行的是第 2 批，不是已经结束的第 1 批。
        assert!(!throttled.advance_throttle(&mut state, at(3699)));
        assert_eq!(state.throttle().released, 2);
        assert!(throttled.advance_throttle(&mut state, at(3700)));
        assert_eq!(state.throttle().released, 3);

        // 第 2 批成交之后链条继续往下走。
        core_fill(&throttled, &mut state, 2, "0.02", at(3710));
        assert_eq!(state.throttle().first_fill_at, Some(at(3710)));
        assert!(throttled.advance_throttle(&mut state, at(7310)));
        assert_eq!(state.throttle().released, 4);
    }

    /// 运行中改成不限速：全部单位立刻放行。
    #[test]
    fn turning_off_a_throttle_releases_every_unit() {
        let throttled = throttled_core_long_strategy();
        let mut state = throttled.initial_state();
        throttled.advance_throttle(&mut state, at(0));
        core_fill(&throttled, &mut state, 0, "0.02", at(10));
        throttled.advance_throttle(&mut state, at(3610));
        assert_eq!(state.throttle().released, 2);

        let unthrottled = core_long_strategy();
        unthrottled.rebuild_throttle(&mut state, at(4000));

        assert_eq!(state.throttle().released, 4);
        assert!(!unthrottled.advance_throttle(&mut state, at(4000)));
    }

    #[test]
    fn an_untracked_grid_cell_is_always_released() {
        // `r > 0`：格子跟随所属批次，本身不是限速单位。
        let strategy = throttled_core_long_strategy();
        let state = strategy.initial_state();

        assert!(strategy.is_released(&state, Unit::Cell(CellIndex(7))));
        assert!(strategy.is_released(&state, Unit::Batch(BatchIndex(0))));
        assert!(!strategy.is_released(&state, Unit::Batch(BatchIndex(1))));

        // `r = 0`：格子就是单位。
        let strategy = throttled_long_grid_strategy();
        let state = strategy.initial_state();
        assert!(strategy.is_released(&state, Unit::Cell(CellIndex(0))));
        assert!(!strategy.is_released(&state, Unit::Cell(CellIndex(1))));
    }

    #[test]
    fn the_released_count_never_passes_the_unit_count() {
        let strategy = throttled_core_long_strategy();
        let mut state = strategy.initial_state();
        state.set_throttle(ThrottleState {
            released: 4,
            released_at: Some(at(0)),
            first_fill_at: Some(at(0)),
        });

        assert!(!strategy.advance_throttle(&mut state, at(1_000_000)));
        assert_eq!(state.throttle().released, 4);
    }

    #[test]
    fn a_long_grid_strategy_without_a_throttle_plans_every_cell() {
        let strategy = long_grid_strategy();
        let state = strategy.initial_state();

        assert_eq!(state.throttle().released, 20);
        let plan = strategy.plan(&state, &[], &[], Price::parse("95500").unwrap());
        assert_eq!(plan.place.len(), 20);
        assert!(!plan.waiting.contains(&WaitReason::WaitingForThrottle));
    }
}
