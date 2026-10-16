//! 同一个策略同一时刻只能被一个调用驱动。
//!
//! 两个调用并发走同一步，会用同一份状态各下一遍单：同一侧出现两张都会成交的订单。

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use common::{account, at, exchange, long_grid_config, start_strategy, throttled};
use poise_runtime::{
    Exchange, ExchangeError, ExchangeFill, ExchangeOrder, ManualClock, OrderLookup, OrderReceipt,
    OrderRequest, Runtime, RuntimeError, RuntimeOptions, SimulatedExchange, Store,
};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::ClientOrderId;
use poise_strategy::units::{Position, Price};

/// 每次调用前先让出一次执行权，好让并发的第二次调用有机会插进来。
struct YieldingExchange {
    inner: Arc<SimulatedExchange>,
}

impl YieldingExchange {
    fn new(inner: Arc<SimulatedExchange>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Exchange for YieldingExchange {
    async fn place_order(&self, request: &OrderRequest) -> Result<OrderReceipt, ExchangeError> {
        tokio::task::yield_now().await;
        self.inner.place_order(request).await
    }

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<(), ExchangeError> {
        tokio::task::yield_now().await;
        self.inner.cancel_order(instrument, client_order_id).await
    }

    async fn working_orders(
        &self,
        instrument: &Instrument,
    ) -> Result<Vec<ExchangeOrder>, ExchangeError> {
        tokio::task::yield_now().await;
        self.inner.working_orders(instrument).await
    }

    async fn order_status(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<OrderLookup, ExchangeError> {
        tokio::task::yield_now().await;
        self.inner.order_status(instrument, client_order_id).await
    }

    async fn fills_since(
        &self,
        instrument: &Instrument,
        since: DateTime<Utc>,
    ) -> Result<Vec<ExchangeFill>, ExchangeError> {
        tokio::task::yield_now().await;
        self.inner.fills_since(instrument, since).await
    }

    async fn position(&self, instrument: &Instrument) -> Result<Position, ExchangeError> {
        tokio::task::yield_now().await;
        self.inner.position(instrument).await
    }

    async fn mark_price(&self, instrument: &Instrument) -> Result<Price, ExchangeError> {
        tokio::task::yield_now().await;
        self.inner.mark_price(instrument).await
    }
}

#[tokio::test]
async fn a_strategy_cannot_be_driven_by_two_callers_at_once() {
    let store = Store::open_in_memory().unwrap();
    let exchange = Arc::new(YieldingExchange::new(exchange()));
    let clock = Arc::new(ManualClock::new(at(0)));
    let runtime = Runtime::with_clock(
        exchange.clone(),
        account(),
        store,
        RuntimeOptions::default(),
        clock.clone(),
    );
    let strategy_id = start_strategy(&runtime, long_grid_config()).await;
    clock.set(at(1));

    let (first, second) = tokio::join!(runtime.step(&strategy_id), runtime.step(&strategy_id),);

    // 恰好一个走完这一步，另一个被挡在门外。
    match (&first, &second) {
        (Ok(_), Err(RuntimeError::Busy(busy))) | (Err(RuntimeError::Busy(busy)), Ok(_)) => {
            assert_eq!(busy, &strategy_id);
        }
        other => panic!("expected one step and one Busy, got {other:?}"),
    }
    // 20 格只挂了一遍单，没有被并发的那一步重复挂一遍。
    assert_eq!(exchange.inner.place_count(), 20);
    assert_eq!(runtime.orders(&strategy_id).unwrap().len(), 20);

    // 走完这一步之后驱动权就释放了：下一步照常。
    clock.set(at(2));
    let report = runtime.step(&strategy_id).await.unwrap();
    assert!(report.placed.is_empty());
    assert_eq!(report.plan.place.len(), 0);
}

/// 改配置和走一步也不能同时进行：两者都读同一份状态、都写同一份状态。
#[tokio::test]
async fn changing_the_config_cannot_race_a_step() {
    let store = Store::open_in_memory().unwrap();
    let exchange = Arc::new(YieldingExchange::new(exchange()));
    let clock = Arc::new(ManualClock::new(at(0)));
    let runtime = Runtime::with_clock(
        exchange.clone(),
        account(),
        store,
        RuntimeOptions::default(),
        clock.clone(),
    );
    let strategy_id = start_strategy(&runtime, long_grid_config()).await;
    clock.set(at(1));

    let (step, update) = tokio::join!(runtime.step(&strategy_id), async {
        runtime.update_config(
            &strategy_id,
            throttled(long_grid_config(), Duration::hours(1)),
        )
    },);

    // 恰好一个走完，另一个被挡在门外。
    match (&step, &update) {
        (Ok(_), Err(RuntimeError::Busy(busy))) | (Err(RuntimeError::Busy(busy)), Ok(_)) => {
            assert_eq!(busy, &strategy_id);
        }
        other => panic!("expected one of the two to be Busy, got {other:?}"),
    }
    assert_eq!(exchange.inner.place_count(), 20, "这一步只挂了一遍单");
    assert_eq!(
        runtime.config_versions(&strategy_id).unwrap().len(),
        1,
        "被挡下的改配置不记版本"
    );

    // 走完这一步之后驱动权就释放了：改配置照常。
    clock.set(at(2));
    let version = runtime
        .update_config(
            &strategy_id,
            throttled(long_grid_config(), Duration::hours(1)),
        )
        .unwrap();
    assert_eq!(version, 2);
}
