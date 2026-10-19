//! 已归档、标的上仍有仓位（第 7.7 节第 8 条）。
//!
//! 已归档的策略不再走步，视图快照停在归档那一刻，不能拿来判断现在还有没有仓位。常驻服务
//! 每分钟按市场读一次这些标的的净仓位和标记价，只读、不下单，读数放在内存里：列表上的
//! 「仍有仓位」和市价平仓按钮按它显示。读不到时沿用上一次的读数；超过
//! [`HELD_POSITION_MAX_AGE`] 没读到新的就当读不到，不显示、也不给平仓按钮。
//!
//! 仓位按标的算，不按策略算：同一标的有几份已归档策略时只挂在最近归档的那一份上；标的上
//! 有未归档策略时仓位归它管，不列出。

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use poise_strategy::Strategy;
use poise_strategy::instrument::Instrument;
use poise_strategy::units::{Position, Price};
use serde::{Deserialize, Serialize};

use crate::store::{StoredStrategy, StrategyId};

use super::{Runtime, RuntimeError};

/// 读数超过这么久没更新，就当读不到。
pub const HELD_POSITION_MAX_AGE: Duration = Duration::minutes(5);

/// 已归档策略的标的上仍有的净仓位：读到的仓位、当时的标记价（确认弹窗按它算最差成交价）
/// 和读到的时刻。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldPosition {
    pub position: Position,
    pub market_price: Price,
    pub read_at: DateTime<Utc>,
}

/// 一个标的最近一次的读数，挂在哪一份已归档策略上。
#[derive(Debug, Clone)]
pub(super) struct HeldReading {
    strategy_id: StrategyId,
    held: HeldPosition,
}

impl Runtime {
    /// 读一次已归档标的的净仓位（常驻服务每分钟一次）。
    ///
    /// 读不到时报错、保留上一次的读数：之后的读数超过 [`HELD_POSITION_MAX_AGE`] 自然不再显示。
    /// 仓位为 0、或者按标记价不足最低下单金额的，不算「仍有仓位」：下不了单，列出来也平不掉。
    pub async fn read_archived_positions(&self) -> Result<(), RuntimeError> {
        let mut live: HashSet<Instrument> = HashSet::new();
        let mut latest: HashMap<Instrument, StoredStrategy> = HashMap::new();
        for strategy_id in self.store.load_all_strategy_ids()? {
            let stored = self.store.load_strategy(&strategy_id)?;
            if !stored.status.is_archived() {
                live.insert(stored.instrument.clone());
                continue;
            }
            match latest.get(&stored.instrument) {
                Some(kept) if kept.updated_at >= stored.updated_at => {}
                _ => {
                    latest.insert(stored.instrument.clone(), stored);
                }
            }
        }
        latest.retain(|instrument, _| !live.contains(instrument));
        // 不再列出的标的（上面有了新策略，或者归档的那一份换了）：旧读数作废。
        self.held_positions().retain(|instrument, reading| {
            latest
                .get(instrument)
                .is_some_and(|stored| stored.strategy_id == reading.strategy_id)
        });
        if latest.is_empty() {
            return Ok(());
        }

        let instruments: Vec<Instrument> = latest.keys().cloned().collect();
        let read_at = self.clock.now();
        let positions = self.exchange.positions(&instruments).await?;
        for (instrument, position) in positions {
            let stored = &latest[&instrument];
            if position.units() == 0 {
                self.held_positions().remove(&instrument);
                continue;
            }
            let market_price = match self.exchange.mark_price(&instrument).await {
                Ok(price) => price,
                Err(error) => {
                    tracing::warn!(%instrument, %error, "no mark price for the archived position");
                    continue;
                }
            };
            let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
            if strategy.market_close(position, market_price, &[]).settled {
                self.held_positions().remove(&instrument);
                continue;
            }
            self.held_positions().insert(
                instrument,
                HeldReading {
                    strategy_id: stored.strategy_id.clone(),
                    held: HeldPosition {
                        position,
                        market_price,
                        read_at,
                    },
                },
            );
        }
        Ok(())
    }

    /// 这份已归档策略的标的上仍有的净仓位；读数过期、不挂在它身上、或者标的上已经有未归档
    /// 策略时是 `None`。最后一条在显示时现查：新策略启动和每分钟的读取可能交错，正在进行的
    /// 读取会把刚作废的旧读数写回去，只靠清缓存挡不住。
    pub(super) fn held_position(&self, stored: &StoredStrategy) -> Option<HeldPosition> {
        if !stored.status.is_archived() {
            return None;
        }
        if !matches!(self.store.live_strategy(&stored.instrument), Ok(None)) {
            return None;
        }
        let now = self.clock.now();
        self.held_positions()
            .get(&stored.instrument)
            .filter(|reading| reading.strategy_id == stored.strategy_id)
            .filter(|reading| now - reading.held.read_at <= HELD_POSITION_MAX_AGE)
            .map(|reading| reading.held.clone())
    }

    fn held_positions(&self) -> std::sync::MutexGuard<'_, HashMap<Instrument, HeldReading>> {
        self.held.lock().expect("held positions lock poisoned")
    }
}
