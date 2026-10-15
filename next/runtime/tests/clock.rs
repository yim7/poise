//! 时钟注入（第 9.1 节）：runtime 从注入的时钟取当前时间，每张订单的 `expiresAfter`
//! 按它**实际发出去的时刻**算，不是这一步开始的时刻。
//!
//! 一步里挂了 20 张单，每张单的发送都花掉 1 秒：套用步首时间的话，后面的单会带着一个
//! 早就过去的时间戳发出去，被交易所当成过期订单拒掉。

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use common::{INITIAL_MARKET_PRICE, at, btc_fees, btc_spec, exchange, long_grid_config};
use poise_runtime::{
    Clock, Exchange, ExchangeError, ExchangeFill, ExchangeOrder, ManualClock, OrderLookup,
    OrderReceipt, OrderRequest, Runtime, RuntimeOptions, SimulatedExchange, Store,
};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderStatus};
use poise_strategy::units::{Position, Price};

/// 发送要花时间的交易所：每次下单让时钟往前走 `per_order`。
///
/// 模拟交易所自己的时间一起往前拨，它的过期判断才和真实情况一致。
struct SlowExchange {
    inner: Arc<SimulatedExchange>,
    clock: Arc<ManualClock>,
    per_order: Duration,
    market_price: Price,
}

impl SlowExchange {
    fn new(inner: Arc<SimulatedExchange>, clock: Arc<ManualClock>, per_order: Duration) -> Self {
        Self {
            inner,
            clock,
            per_order,
            market_price: Price::parse(INITIAL_MARKET_PRICE).unwrap(),
        }
    }
}

#[async_trait]
impl Exchange for SlowExchange {
    async fn place_order(&self, request: &OrderRequest) -> Result<OrderReceipt, ExchangeError> {
        self.clock.advance(self.per_order);
        self.inner
            .set_market_price(self.market_price, self.clock.now());
        self.inner.place_order(request).await
    }

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<(), ExchangeError> {
        self.inner.cancel_order(instrument, client_order_id).await
    }

    async fn working_orders(
        &self,
        instrument: &Instrument,
    ) -> Result<Vec<ExchangeOrder>, ExchangeError> {
        self.inner.working_orders(instrument).await
    }

    async fn order_status(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<OrderLookup, ExchangeError> {
        self.inner.order_status(instrument, client_order_id).await
    }

    async fn fills_since(
        &self,
        instrument: &Instrument,
        since: DateTime<Utc>,
    ) -> Result<Vec<ExchangeFill>, ExchangeError> {
        self.inner.fills_since(instrument, since).await
    }

    async fn position(&self, instrument: &Instrument) -> Result<Position, ExchangeError> {
        self.inner.position(instrument).await
    }

    async fn mark_price(&self, instrument: &Instrument) -> Result<Price, ExchangeError> {
        self.inner.mark_price(instrument).await
    }
}

#[tokio::test]
async fn every_order_expires_from_its_own_send_time() {
    let store = Store::open_in_memory().unwrap();
    let sim = exchange();
    let clock = Arc::new(ManualClock::new(at(0)));
    let slow = Arc::new(SlowExchange::new(
        sim.clone(),
        clock.clone(),
        Duration::seconds(1),
    ));
    let runtime = Runtime::with_clock(slow, store, RuntimeOptions::default(), clock.clone());

    // runtime 的当前时间就是注入时钟的时间。
    assert_eq!(runtime.now(), at(0));
    let strategy_id = runtime
        .create_strategy(long_grid_config(), btc_spec(), btc_fees())
        .unwrap();

    let report = runtime.step(&strategy_id).await.unwrap();
    assert_eq!(report.placed.len(), 20);

    let mut orders = runtime.orders(&strategy_id).unwrap();
    assert_eq!(orders.len(), 20);
    orders.sort_by_key(|order| order.placed_at);

    // 时间真的在下单过程中走掉了：每张单的发送时刻各不相同。
    for (index, order) in orders.iter().enumerate() {
        assert_eq!(
            order.placed_at,
            at(index as i64),
            "第 {} 张单在自己的时刻发出",
            index + 1
        );
        assert_eq!(
            order.expires_at,
            order.placed_at + Duration::seconds(5),
            "`expiresAfter` 按这张单实际发出去的时刻算"
        );
        assert_ne!(
            order.record.status,
            OrderStatus::Rejected,
            "第 {} 张单带着已经过去的时间戳发出去了",
            index + 1
        );
    }
    let last = orders.last().unwrap();
    assert_eq!(last.placed_at, at(19));
    assert_eq!(last.expires_at, at(24));
    assert!(last.placed_at > at(0));
}
