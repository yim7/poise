//! 反复撤挂检测：同一价格上的挂单一再被规划撤掉、一笔都没成交（第 14 节的后续事项）。
//!
//! 正常运行时，一张挂单被撤掉要么是价格让它不再需要（限速、批次、反向订单调整），要么是
//! 暂停、停止。同一价格同一方向在一小时里撤了好几次都没成交，多半是程序在「撤了又挂、
//! 挂了又撤」：每次都白白花掉两次操作额度，挂单也一直排不到前面。只提醒，不暂停——
//! 挂单本身没有错，停下来反而丢掉了网格。
//!
//! 只数常规一步里规划撤掉的单：暂停、停止、失效的撤单是用户或止损决定的，不算。记录只在
//! 内存里，重启清零。

use chrono::{DateTime, Duration, Utc};
use poise_strategy::order::Side;
use poise_strategy::units::Price;

use crate::store::StrategyId;

use super::Runtime;

/// 在这段时间里数撤挂次数。
pub(super) const CHURN_WINDOW: Duration = Duration::hours(1);

/// 同一价格同一方向撤到这么多次就提醒。
pub(super) const CHURN_LIMIT: usize = 5;

/// 一个策略最近被撤掉、没成交的挂单。
#[derive(Debug, Default)]
pub(super) struct Churn {
    canceled: Vec<(Side, Price, DateTime<Utc>)>,
    /// 已经提醒过的价位和提醒时刻：一个窗口里只提醒一次。
    alerted: Vec<(Side, Price, DateTime<Utc>)>,
}

impl Churn {
    fn forget_before(&mut self, since: DateTime<Utc>) {
        self.canceled.retain(|(_, _, at)| *at >= since);
        self.alerted.retain(|(_, _, at)| *at >= since);
    }

    fn count(&self, side: Side, price: Price) -> usize {
        self.canceled
            .iter()
            .filter(|(s, p, _)| *s == side && *p == price)
            .count()
    }

    /// 窗口里撤到上限的价位，按第一次出现的先后。
    fn churning(&self) -> Vec<(Side, Price, usize)> {
        let mut found: Vec<(Side, Price, usize)> = Vec::new();
        for (side, price, _) in &self.canceled {
            if found.iter().any(|(s, p, _)| s == side && p == price) {
                continue;
            }
            let count = self.count(*side, *price);
            if count >= CHURN_LIMIT {
                found.push((*side, *price, count));
            }
        }
        found
    }
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Buy => "买",
        Side::Sell => "卖",
    }
}

impl Runtime {
    /// 记下一张规划撤掉、一笔都没成交的挂单；同一价位撤到上限时提醒一次。
    pub(super) fn record_churn(&self, strategy_id: &StrategyId, side: Side, price: Price) {
        let now = self.clock.now();
        let alert = {
            let mut churn = self.churn.lock().expect("churn lock poisoned");
            let churn = churn.entry(strategy_id.clone()).or_default();
            churn.forget_before(now - CHURN_WINDOW);
            churn.canceled.push((side, price, now));
            let count = churn.count(side, price);
            let alerted = churn
                .alerted
                .iter()
                .any(|(s, p, _)| *s == side && *p == price);
            if count >= CHURN_LIMIT && !alerted {
                churn.alerted.push((side, price, now));
                Some(count)
            } else {
                None
            }
        };
        if let Some(count) = alert {
            tracing::warn!(
                strategy = %strategy_id,
                ?side,
                %price,
                count,
                "the same order keeps being canceled without a fill"
            );
            self.notify_strategy(
                strategy_id,
                &format!("churn:{side:?}:{price}"),
                "反复撤挂",
                format!(
                    "{} {price} 一小时内撤了 {count} 次都没成交，可能是程序缺陷：检查挂单和日志。",
                    side_label(side)
                ),
            );
        }
    }

    /// 页面上的提醒：最近一小时里撤到上限的价位。
    pub(super) fn churn_note(&self, strategy_id: &StrategyId) -> Option<String> {
        let now = self.clock.now();
        let mut churn = self.churn.lock().expect("churn lock poisoned");
        let churn = churn.get_mut(strategy_id)?;
        churn.forget_before(now - CHURN_WINDOW);
        let found = churn.churning();
        if found.is_empty() {
            return None;
        }
        let prices: Vec<String> = found
            .iter()
            .map(|(side, price, count)| format!("{} {price} 撤了 {count} 次", side_label(*side)))
            .collect();
        Some(format!(
            "一小时内同一价格反复撤挂都没成交（{}），可能是程序缺陷：检查挂单和日志。",
            prices.join("、")
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, TimeZone, Utc};
    use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
    use poise_strategy::order::Side;
    use poise_strategy::units::{Notional, Price, Quantity, Rate};

    use super::CHURN_LIMIT;
    use crate::clock::ManualClock;
    use crate::notify::RecordingNotifier;
    use crate::store::{Store, StrategyId};
    use crate::{Runtime, RuntimeOptions, SimulatedAccount, SimulatedExchange};

    fn runtime() -> (Runtime, Arc<ManualClock>, Arc<RecordingNotifier>) {
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let btc = Instrument::try_new("BTC").unwrap();
        let spec = InstrumentSpec::new(
            btc.clone(),
            Price::parse("1").unwrap(),
            Quantity::parse("0.001").unwrap(),
            Notional::parse("10").unwrap(),
        )
        .unwrap();
        let fees = Fees::new(Rate::ZERO, Rate::ZERO);
        let clock = Arc::new(ManualClock::new(start));
        let notices = RecordingNotifier::new();
        let runtime = Runtime::with_clock(
            Arc::new(SimulatedExchange::new(
                btc,
                Price::parse("100").unwrap(),
                start,
            )),
            Arc::new(SimulatedAccount::new(spec, fees)),
            Store::open_in_memory().unwrap(),
            RuntimeOptions::default(),
            clock.clone(),
        )
        .with_notifier(notices.clone());
        (runtime, clock, notices)
    }

    /// 同一价格同一方向一小时内撤到上限才提醒，只提醒一次；页面说明列出这个价位。别的价位、
    /// 别的方向分开数，过了一小时重新数。
    #[test]
    fn the_same_price_canceled_again_and_again_is_flagged_once() {
        let (runtime, clock, notices) = runtime();
        let strategy = StrategyId::new();
        let price = Price::parse("95000").unwrap();

        for _ in 1..CHURN_LIMIT {
            runtime.record_churn(&strategy, Side::Buy, price);
            runtime.record_churn(&strategy, Side::Sell, price);
            runtime.record_churn(&strategy, Side::Buy, Price::parse("94000").unwrap());
            clock.advance(Duration::minutes(1));
        }
        assert!(notices.notices().is_empty());
        assert_eq!(runtime.churn_note(&strategy), None);

        runtime.record_churn(&strategy, Side::Buy, price);
        runtime.record_churn(&strategy, Side::Buy, price);

        let titles = notices.titles();
        assert_eq!(titles.len(), 1, "{titles:?}");
        assert!(titles[0].ends_with("反复撤挂"), "{titles:?}");
        assert!(notices.notices()[0].body.contains("买 95000"));
        let note = runtime.churn_note(&strategy).unwrap();
        assert!(note.contains("买 95000 撤了 6 次"), "{note}");

        clock.advance(Duration::hours(2));
        assert_eq!(runtime.churn_note(&strategy), None);
    }
}
