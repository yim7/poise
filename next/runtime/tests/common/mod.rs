//! 集成测试共用的夹具：设计第 3.2 节的例子。
//!
//! BTC，报价精度 1，数量精度 0.001，最低订单金额 10；做多网格 75000–95000，
//! M = 1，间距 1000，价差 1000，得到 20 格、每格 0.05。

#![allow(dead_code)]

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use poise_runtime::{
    ManualClock, ResumeOutcome, Runtime, RuntimeOptions, SimulatedExchange, StepReport, Store,
    StoredOrder, StrategyId,
};
use poise_strategy::batch::BatchIndex;
use poise_strategy::config::{SideConfig, StrategyConfig};
use poise_strategy::grid::CellIndex;
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::order::{ClientOrderId, OrderPurpose, Unit};
use poise_strategy::state::CellState;
use poise_strategy::units::{Notional, Price, Quantity, Rate};

/// 起始标记价落在网格最高买价与最低卖价之间：买单和卖单挂上时都不会立刻成交。
pub const INITIAL_MARKET_PRICE: &str = "95500";

pub fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap() + chrono::Duration::seconds(seconds)
}

pub fn btc() -> Instrument {
    Instrument::try_new("BTC").unwrap()
}

pub fn btc_spec() -> InstrumentSpec {
    InstrumentSpec::new(
        btc(),
        Price::parse("1").unwrap(),
        Quantity::parse("0.001").unwrap(),
        Notional::parse("10").unwrap(),
    )
    .unwrap()
}

pub fn btc_fees() -> Fees {
    Fees::new(
        Rate::parse("0.00045").unwrap(),
        Rate::parse("0.00015").unwrap(),
    )
}

pub fn long_grid_config() -> StrategyConfig {
    StrategyConfig {
        instrument: btc(),
        long: SideConfig {
            price_low: Price::parse("75000").unwrap(),
            price_high: Price::parse("95000").unwrap(),
            max_position: Quantity::parse("1").unwrap(),
            core_ratio: Rate::ZERO,
            batch_count: None,
            grid_spacing: Price::parse("1000").unwrap(),
            grid_spread: Price::parse("1000").unwrap(),
            throttle_interval: None,
        },
    }
}

/// 加上限速：同样的划分，`τ = 1` 小时。
pub fn throttled(config: StrategyConfig, interval: chrono::Duration) -> StrategyConfig {
    StrategyConfig {
        long: SideConfig {
            throttle_interval: Some(interval),
            ..config.long
        },
        ..config
    }
}

/// 同一个网格加上主仓：`r = .7`，`n = 4`。
///
/// 四批的主仓价是 95000、90000、85000、80000，每批 `C = .175`、`G = .075`，
/// 跟随每批的 5 格各 `.015`。
pub fn core_long_config() -> StrategyConfig {
    StrategyConfig {
        long: SideConfig {
            core_ratio: Rate::parse("0.7").unwrap(),
            batch_count: Some(4),
            ..long_grid_config().long
        },
        ..long_grid_config()
    }
}

/// `r = 1`：只积累主仓，没有网格（第 2.3 节）。
///
/// 四批的主仓价是 95000、90000、85000、80000，每批 `q = C = .25`，没有格子。
pub fn core_only_config() -> StrategyConfig {
    StrategyConfig {
        long: SideConfig {
            core_ratio: Rate::parse("1").unwrap(),
            batch_count: Some(4),
            ..long_grid_config().long
        },
        ..long_grid_config()
    }
}

pub struct Harness {
    pub exchange: Arc<SimulatedExchange>,
    pub runtime: Runtime,
    /// 测试自己控制的时钟：`step`、`pause`、`resume` 都先把拨到指定时刻再调用。
    pub clock: Arc<ManualClock>,
    pub strategy_id: StrategyId,
}

pub fn exchange() -> Arc<SimulatedExchange> {
    Arc::new(SimulatedExchange::new(
        btc(),
        Price::parse(INITIAL_MARKET_PRICE).unwrap(),
        at(0),
    ))
}

pub fn harness() -> Harness {
    harness_with(long_grid_config(), Store::open_in_memory().unwrap())
}

pub fn core_harness() -> Harness {
    harness_with(core_long_config(), Store::open_in_memory().unwrap())
}

pub fn core_only_harness() -> Harness {
    harness_with(core_only_config(), Store::open_in_memory().unwrap())
}

pub fn harness_with_store(store: Store) -> Harness {
    harness_with(long_grid_config(), store)
}

pub fn harness_with(config: StrategyConfig, store: Store) -> Harness {
    let exchange = exchange();
    // 每次下单都查一次库：这张单必须在发出去之前就已经落盘（第 9.1 节）。
    exchange.probe_stored_orders(store.clone());
    let clock = Arc::new(ManualClock::new(at(0)));
    let runtime = Runtime::with_clock(
        exchange.clone(),
        store,
        RuntimeOptions::default(),
        clock.clone(),
    );
    let strategy_id = runtime
        .create_strategy(config, btc_spec(), btc_fees())
        .unwrap();
    Harness {
        exchange,
        runtime,
        clock,
        strategy_id,
    }
}

/// 进程重启：交易所还在（同一个模拟交易所），数据库重新打开，策略沿用原来的标识。
pub fn restart(store: Store, exchange: Arc<SimulatedExchange>, strategy_id: StrategyId) -> Harness {
    exchange.probe_stored_orders(store.clone());
    let clock = Arc::new(ManualClock::new(at(0)));
    Harness {
        runtime: Runtime::with_clock(
            exchange.clone(),
            store,
            RuntimeOptions::default(),
            clock.clone(),
        ),
        exchange,
        clock,
        strategy_id,
    }
}

impl Harness {
    /// 拨到 `seconds`，再走一步。
    pub async fn step(&self, seconds: i64) -> StepReport {
        self.clock.set(at(seconds));
        self.runtime.step(&self.strategy_id).await.unwrap()
    }

    /// 拨到 `seconds`，再暂停。
    pub async fn pause(&self, seconds: i64) -> Vec<ClientOrderId> {
        self.clock.set(at(seconds));
        self.runtime.pause(&self.strategy_id).await.unwrap()
    }

    /// 拨到 `seconds`，再恢复。
    pub async fn resume(&self, seconds: i64) -> ResumeOutcome {
        self.clock.set(at(seconds));
        self.runtime.resume(&self.strategy_id).await.unwrap()
    }

    pub fn orders(&self) -> Vec<StoredOrder> {
        self.runtime.orders(&self.strategy_id).unwrap()
    }

    /// 当前还在场的订单记录。
    pub fn working(&self) -> Vec<StoredOrder> {
        self.orders()
            .into_iter()
            .filter(|order| order.record.is_working())
            .collect()
    }

    /// 某个格子某一侧当前在场订单的 `cloid`。
    pub fn working_cloid(&self, cell: usize, purpose: OrderPurpose) -> ClientOrderId {
        self.unit_cloid(Unit::Cell(CellIndex(cell)), purpose)
    }

    /// 某一批主仓当前在场订单的 `cloid`。
    pub fn working_core_cloid(&self, batch: usize) -> ClientOrderId {
        self.unit_cloid(Unit::Batch(BatchIndex(batch)), OrderPurpose::CoreBuy)
    }

    /// 按 `cloid` 取订单记录。
    pub fn order(&self, client_order_id: &ClientOrderId) -> StoredOrder {
        self.orders()
            .into_iter()
            .find(|order| &order.record.client_order_id == client_order_id)
            .unwrap_or_else(|| panic!("no order `{client_order_id}`"))
    }

    fn unit_cloid(&self, unit: Unit, purpose: OrderPurpose) -> ClientOrderId {
        let matching: Vec<ClientOrderId> = self
            .working()
            .into_iter()
            .filter(|order| order.record.unit == unit && order.record.purpose == purpose)
            .map(|order| order.record.client_order_id)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "expected exactly one working {purpose:?} order in {unit:?}"
        );
        matching[0].clone()
    }

    pub fn cell(&self, cell: usize) -> CellState {
        self.runtime
            .state(&self.strategy_id)
            .unwrap()
            .cell(CellIndex(cell))
            .unwrap()
            .clone()
    }

    /// 某一批的主仓累计成交。
    pub fn core_filled(&self, batch: usize) -> Quantity {
        self.runtime
            .state(&self.strategy_id)
            .unwrap()
            .batch(BatchIndex(batch))
            .unwrap()
            .filled
    }

    /// 交易所上某张挂单。
    pub fn at_exchange(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Option<poise_runtime::ExchangeOrder> {
        self.exchange
            .orders()
            .into_iter()
            .find(|order| &order.client_order_id == client_order_id)
    }
}
