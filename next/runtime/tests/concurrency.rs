//! 同一个策略同一时刻只能被一个调用驱动。
//!
//! 两个调用并发走同一步，会用同一份状态各下一遍单：同一侧出现两张都会成交的订单。

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{at, btc_fees, btc_spec, exchange, long_grid_config};
use poise_runtime::{
    Exchange, ExchangeError, ExchangeFill, ExchangeOrder, OrderLookup, OrderReceipt, OrderRequest,
    Runtime, RuntimeError, SimulatedExchange, Store,
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
    let runtime = Runtime::new(exchange.clone(), store);
    let strategy_id = runtime
        .create_strategy(long_grid_config(), btc_spec(), btc_fees(), at(0))
        .unwrap();

    let (first, second) = tokio::join!(
        runtime.step(&strategy_id, at(1)),
        runtime.step(&strategy_id, at(1)),
    );

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
    let report = runtime.step(&strategy_id, at(2)).await.unwrap();
    assert!(report.placed.is_empty());
    assert_eq!(report.plan.place.len(), 0);
}
