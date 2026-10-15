//! 时钟：runtime 取当前时间的唯一来源。
//!
//! 策略本身不含时间（纯计算）。runtime 需要时间的地方都从这里取：
//! 一张订单的 `expiresAfter` 按它**实际发送时刻**算（第 9.1 节），限速的放行时刻按
//! **实际成交时刻**算（第 4.1 节）。把时间做成可注入的，这两件事在测试里就能精确
//! 控制，不依赖测试跑得多快。

use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

/// 当前时间的来源。
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// 真实时钟。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// 测试用的可控时钟：只有显式拨动才会走。
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    /// 拨到某个时刻。
    pub fn set(&self, now: DateTime<Utc>) {
        *self.now.lock().expect("clock lock poisoned") = now;
    }

    /// 往前走一段。
    pub fn advance(&self, by: Duration) {
        self.set(self.now() + by);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("clock lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, ManualClock, SystemClock};
    use chrono::{DateTime, Duration, TimeZone, Utc};

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap() + Duration::seconds(seconds)
    }

    #[test]
    fn a_manual_clock_only_moves_when_it_is_told_to() {
        let clock = ManualClock::new(at(0));
        assert_eq!(clock.now(), at(0));
        clock.advance(Duration::minutes(5));
        assert_eq!(clock.now(), at(300));
        clock.set(at(1));
        assert_eq!(clock.now(), at(1));
    }

    #[test]
    fn the_system_clock_is_close_to_now() {
        let before = Utc::now();
        let reading = SystemClock.now();
        assert!(reading >= before && reading <= Utc::now());
    }
}
