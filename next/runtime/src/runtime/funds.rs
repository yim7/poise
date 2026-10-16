//! 资金预检与保证金不足的退避（第 10 节）。
//!
//! 每张会增加敞口的订单（非只减仓）下单前做一次保守预检：所需保证金不超过账户可用资金
//! 减去本系统其他在场同类订单的估算。同一账户的这类订单经同一把锁依次预检，不会两个
//! 策略同时用掉同一份额度。交易所以保证金不足拒绝之后，这个单位按 1、2、4、8 秒……最多
//! 60 秒退避，不每一步都再撞一次。

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use poise_strategy::Strategy;
use poise_strategy::funds::{fits, margin_required};
use poise_strategy::instrument::{Fees, Instrument};
use poise_strategy::order::{OrderPurpose, Unit};
use poise_strategy::units::{Notional, Price, Quantity};

use crate::store::StrategyId;

use super::{Runtime, RuntimeError};

/// 退避的上限：最多每 60 秒试一次（第 10 节）。
const MAX_BACKOFF_SECONDS: i64 = 60;

/// 资金预检用的锁和退避表。一个运行时一份：同一账户的所有策略共用。
#[derive(Default)]
pub(super) struct Funds {
    /// 预检和下单之间不能插进别的策略的预检，否则两边都按同一份可用资金放行。
    placing: tokio::sync::Mutex<()>,
    backoff: Mutex<HashMap<BackoffKey, Backoff>>,
}

type BackoffKey = (StrategyId, Unit, OrderPurpose);

#[derive(Debug, Clone, Copy)]
struct Backoff {
    attempts: u32,
    until: DateTime<Utc>,
}

/// 一次预检读到的额度：可用资金、本系统在场增仓单已经占用的估算，以及这个标的的杠杆。
pub(super) struct FundsCheck {
    available: Notional,
    reserved: Notional,
    leverage: u32,
    fees: Fees,
}

impl FundsCheck {
    /// 这张单放得下就记上它占用的额度，返回记上的数；放不下返回 `None`，额度不动。
    pub(super) fn reserve(&mut self, quantity: Quantity, limit_price: Price) -> Option<Notional> {
        let required = margin_required(quantity, limit_price, self.leverage, &self.fees);
        if !fits(self.available, self.reserved, required) {
            return None;
        }
        self.reserved = self.reserved + required;
        Some(required)
    }

    /// 预留了但没发出去（行情过期、写库后有效期不够）或者被拒绝：把额度还回去。
    pub(super) fn release(&mut self, required: Notional) {
        self.reserved = self.reserved - required;
    }
}

impl Runtime {
    /// 拿到预检的锁：持有期间别的策略不能预检、不能下增仓单。
    pub(super) async fn lock_funds(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.funds.placing.lock().await
    }

    /// 读一次额度：可用资金每次都重新读（用户在现货上的买卖会改变它），已占用的部分按
    /// 本系统全部策略在场的增仓单估算。
    ///
    /// 调用方必须先拿到 [`Runtime::lock_funds`] 的锁。
    pub(super) async fn funds_check(
        &self,
        strategy: &Strategy,
    ) -> Result<FundsCheck, RuntimeError> {
        let instrument = strategy.spec().instrument();
        let mut leverages: HashMap<Instrument, u32> = HashMap::new();
        let leverage = self.account.leverage(instrument).await?;
        leverages.insert(instrument.clone(), leverage);

        // 先算已占用、后读可用资金。对账不拿这把锁：两次读取之间别的策略可能有单成交。
        // 先扫订单，那张单还按在场全额算着，交易所的可用资金又已经扣过它，最多是重复算、
        // 偏保守；反过来先读资金，成交扣掉的那部分在两边都看不见，就会多放行。
        let mut reserved = Notional::ZERO;
        for strategy_id in self.store.load_all_strategy_ids()? {
            let stored = self.store.load_strategy(&strategy_id)?;
            let increasing: Vec<_> = self
                .store
                .load_orders(&strategy_id)?
                .into_iter()
                .filter(|order| order.record.is_working() && !order.record.reduce_only)
                .collect();
            if increasing.is_empty() {
                continue;
            }
            let leverage = match leverages.get(&stored.instrument) {
                Some(leverage) => *leverage,
                None => {
                    let leverage = self.account.leverage(&stored.instrument).await?;
                    leverages.insert(stored.instrument.clone(), leverage);
                    leverage
                }
            };
            for order in increasing {
                reserved = reserved
                    + margin_required(
                        order.record.remaining(),
                        order.record.limit_price,
                        leverage,
                        &stored.fees,
                    );
            }
        }
        let available = self.account.available_collateral().await?;
        Ok(FundsCheck {
            available,
            reserved,
            leverage,
            fees: *strategy.fees(),
        })
    }

    /// 这个单位还在保证金不足的退避期里吗。
    pub(super) fn in_backoff(
        &self,
        strategy_id: &StrategyId,
        unit: Unit,
        purpose: OrderPurpose,
        now: DateTime<Utc>,
    ) -> bool {
        self.funds
            .backoff
            .lock()
            .expect("backoff lock poisoned")
            .get(&(strategy_id.clone(), unit, purpose))
            .is_some_and(|backoff| now < backoff.until)
    }

    /// 交易所以保证金不足拒绝了这个单位的单：下次重试往后推 1、2、4、8 秒……最多 60 秒。
    pub(super) fn back_off(
        &self,
        strategy_id: &StrategyId,
        unit: Unit,
        purpose: OrderPurpose,
        now: DateTime<Utc>,
    ) {
        let mut backoff = self.funds.backoff.lock().expect("backoff lock poisoned");
        let entry = backoff
            .entry((strategy_id.clone(), unit, purpose))
            .or_insert(Backoff {
                attempts: 0,
                until: now,
            });
        entry.attempts = entry.attempts.saturating_add(1);
        entry.until = now + backoff_delay(entry.attempts);
    }

    /// 这个单位的单发出去了：退避结束。
    pub(super) fn clear_backoff(
        &self,
        strategy_id: &StrategyId,
        unit: Unit,
        purpose: OrderPurpose,
    ) {
        self.funds
            .backoff
            .lock()
            .expect("backoff lock poisoned")
            .remove(&(strategy_id.clone(), unit, purpose));
    }
}

/// 第 `attempts` 次被拒之后等多久：1、2、4、8 秒……最多 60 秒。
fn backoff_delay(attempts: u32) -> Duration {
    // 2^6 = 64 已经超过上限：指数到这里就不用再算，免得移位溢出。
    let exponent = attempts.saturating_sub(1);
    if exponent >= 6 {
        return Duration::seconds(MAX_BACKOFF_SECONDS);
    }
    Duration::seconds((1_i64 << exponent).min(MAX_BACKOFF_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::backoff_delay;
    use chrono::Duration;

    #[test]
    fn the_backoff_doubles_up_to_a_minute() {
        let delays: Vec<i64> = (1..=8)
            .map(|attempts| backoff_delay(attempts).num_seconds())
            .collect();

        assert_eq!(delays, vec![1, 2, 4, 8, 16, 32, 60, 60]);
        for attempts in [63, 64, 65, 1000, u32::MAX] {
            assert_eq!(backoff_delay(attempts), Duration::seconds(60));
        }
    }
}
