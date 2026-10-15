//! 时钟注入（第 9.1 节）：runtime 从注入的时钟取当前时间。
//!
//! 两个时刻都必须是「事情真正发生的那一刻」：
//! - 订单的 `expiresAfter` 按这张单**实际发出去的时刻**算，不是这一步开始的时刻。
//!   一步里挂了 20 张单，每张单的发送都花掉 1 秒：套用步首时间的话，后面的单会带着一个
//!   早就过去的时间戳发出去，被交易所当成过期订单拒掉。
//! - 限速的首笔成交时刻按成交**真正发生的时刻**算，不是下单的时刻。交易所的回执没有
//!   时间戳，只能用收到回执的时刻作保守上界，真实流水到了再校正（第 4.1 节）。
//!   终态订单补记成交时同理：用拿到订单状态的时刻，不是这一步开始的时刻——查询发生在
//!   这一步中途，步首时刻可能早于真实成交时刻，记下来之后真实流水也改不回去。

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use common::{
    INITIAL_MARKET_PRICE, at, btc, btc_fees, btc_spec, exchange, long_grid_config, long_state,
    throttled,
};
use poise_runtime::{
    Clock, Exchange, ExchangeError, ExchangeFill, ExchangeOrder, ManualClock, OrderLookup,
    OrderReceipt, OrderRequest, Runtime, RuntimeOptions, SimulatedExchange, Store,
};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderStatus};
use poise_strategy::units::{Position, Price, Quantity};

/// 调用要花时间的交易所：每次下单让时钟往前走 `per_order`，每次查订单状态往前走
/// `per_query`。
///
/// 模拟交易所自己的时间一起往前拨，它的过期判断才和真实情况一致。
struct SlowExchange {
    inner: Arc<SimulatedExchange>,
    clock: Arc<ManualClock>,
    per_order: Duration,
    per_query: Duration,
    market_price: Price,
}

impl SlowExchange {
    fn new(inner: Arc<SimulatedExchange>, clock: Arc<ManualClock>, per_order: Duration) -> Self {
        Self {
            inner,
            clock,
            per_order,
            per_query: Duration::zero(),
            market_price: Price::parse(INITIAL_MARKET_PRICE).unwrap(),
        }
    }

    /// 换一个标记价：让某些挂单在下单时就被穿过，构造「下单即成交」。
    fn with_market_price(mut self, market_price: Price) -> Self {
        self.market_price = market_price;
        self
    }

    /// 每次查订单状态也让时钟往前走 `per_query`：查询结果到手时已经晚了一截。
    fn with_slow_queries(mut self, per_query: Duration) -> Self {
        self.per_query = per_query;
        self
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
        self.clock.advance(self.per_query);
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

/// 每一步读时钟都往前走一段的时钟：模拟「取到时刻之后、真正发出去之前」花掉的时间。
///
/// `ManualClock` 不会自己走，构造不出这种情况；落库慢一点、系统负载高一点都会让
/// 写库前后的两次读数不一样。
struct TickingClock {
    now: std::sync::Mutex<DateTime<Utc>>,
    per_read: std::sync::Mutex<Duration>,
}

impl TickingClock {
    fn new(start: DateTime<Utc>, per_read: Duration) -> Self {
        Self {
            now: std::sync::Mutex::new(start),
            per_read: std::sync::Mutex::new(per_read),
        }
    }

    /// 不再随读取前进：构造出一次「有效期不够」之后，让下一步正常下单。
    fn stop(&self) {
        *self.per_read.lock().unwrap() = Duration::zero();
    }
}

impl Clock for TickingClock {
    fn now(&self) -> DateTime<Utc> {
        let mut now = self.now.lock().unwrap();
        let current = *now;
        *now = current + *self.per_read.lock().unwrap();
        current
    }
}

/// `expiresAfter` 在写库前取时：落库花掉的时间里订单白白少活一段。
///
/// 发送前按当时的时钟重算到期时刻，订单才真的从「发出去的那一刻」开始活 `expiresAfter`。
#[tokio::test]
async fn an_order_is_sent_with_a_fresh_expiry_after_the_write() {
    let store = Store::open_in_memory().unwrap();
    let clock = Arc::new(TickingClock::new(at(0), Duration::seconds(1)));
    let runtime = Runtime::with_clock(exchange(), store, RuntimeOptions::default(), clock.clone());
    let strategy_id = runtime
        .create_strategy(long_grid_config(), btc_spec(), btc_fees())
        .unwrap();

    let report = runtime.step(&strategy_id).await.unwrap();
    assert_eq!(report.placed.len(), 20);

    let mut orders = runtime.orders(&strategy_id).unwrap();
    orders.sort_by_key(|order| order.placed_at);
    for (index, order) in orders.iter().enumerate() {
        assert_ne!(
            order.record.status,
            OrderStatus::Rejected,
            "第 {} 张单带着一个已经过去的时间戳发出去了",
            index + 1
        );
        assert!(
            order.expires_at > order.placed_at + Duration::seconds(5),
            "第 {} 张单的到期时刻没有按发送前的时钟重算",
            index + 1
        );
    }
}

/// 下单即成交时，限速的首笔成交时刻取收到回执的时刻，不是发送时刻。
///
/// 交易所的回执没有时间戳，只能取一个保守上界：成交发生在发送之后、收到回执之前，
/// 所以收到回执的时刻一定不早于真实成交时刻。发送本身花掉 1 秒时，这两个时刻差 1 秒。
#[tokio::test]
async fn an_immediate_fill_is_dated_by_the_receipt_not_the_send() {
    let store = Store::open_in_memory().unwrap();
    let sim = exchange();
    let clock = Arc::new(ManualClock::new(at(0)));
    // 市价 95000 正好等于第 0 格的买价：它的买单一发出去就成交。
    let slow = Arc::new(
        SlowExchange::new(sim.clone(), clock.clone(), Duration::seconds(1))
            .with_market_price(Price::parse("95000").unwrap()),
    );
    let runtime = Runtime::with_clock(slow, store, RuntimeOptions::default(), clock.clone());
    let strategy_id = runtime
        .create_strategy(
            throttled(long_grid_config(), Duration::hours(1)),
            btc_spec(),
            btc_fees(),
        )
        .unwrap();

    let report = runtime.step(&strategy_id).await.unwrap();
    assert_eq!(report.placed.len(), 1, "限速之下只有第 0 格挂单");

    let state = runtime.state(&strategy_id).unwrap();
    assert_eq!(long_state(&state).throttle().released, 1);
    assert_eq!(
        long_state(&state).throttle().first_fill_at,
        Some(at(1)),
        "回执没有时间戳：取收到回执的时刻，不是发送时刻 at(0)"
    );
}

/// 写库之后有效期还是不够：这一步不发送这张单，也不留下「可能已发送」的记录。
///
/// 发送之前按当时的时钟重算到期时刻，重算和落库本身也要花时间。写完之后有效期仍然
/// 不够时，发出去只会带着一个已经过去的到期时刻被交易所拒掉，而这一步已经没有时间
/// 再重写一次；把记录删掉，下一步重新规划（数量和价格都可能已经变了）。
#[tokio::test]
async fn an_order_whose_refreshed_expiry_is_used_up_is_not_sent() {
    let store = Store::open_in_memory().unwrap();
    let sim = exchange();
    sim.probe_stored_orders(store.clone());
    // 每读一次时钟走 10 秒，`expiresAfter` 只有 5 秒：重算和落库花掉的时间里就过期了。
    let clock = Arc::new(TickingClock::new(at(0), Duration::seconds(10)));
    let runtime = Runtime::with_clock(sim.clone(), store, RuntimeOptions::default(), clock.clone());
    let strategy_id = runtime
        .create_strategy(long_grid_config(), btc_spec(), btc_fees())
        .unwrap();

    let report = runtime.step(&strategy_id).await.unwrap();

    assert!(report.placed.is_empty(), "有效期不够的订单不该发出去");
    assert_eq!(sim.place_count(), 0, "这张单根本没有发到交易所");
    assert!(
        runtime.orders(&strategy_id).unwrap().is_empty(),
        "订单记录回到「还没发送」之前的样子"
    );

    // 下一步重新规划：时钟不再吞掉有效期，二十格照常挂出去。
    clock.stop();
    let report = runtime.step(&strategy_id).await.unwrap();
    assert_eq!(report.placed.len(), 20);
    assert_eq!(sim.place_count(), 20);
}

/// 终态订单补记成交时，成交时刻取拿到订单状态的时刻，不是这一步开始的时刻。
///
/// 成交发生在这一步中途（t = 95），订单状态查到手里时已经是 t = 100。用步首的 t = 90
/// 会记出一个早于真实成交的时刻；限速的首笔成交时刻只往早里改
/// （[`poise_strategy::Strategy::record_fill_time`]），真实流水到了也修不回来，
/// 下一个单位会提前放行。
#[tokio::test]
async fn a_terminal_fill_is_dated_by_the_query_receipt_not_the_step_start() {
    let store = Store::open_in_memory().unwrap();
    let sim = exchange();
    let clock = Arc::new(ManualClock::new(at(0)));
    let slow = Arc::new(
        SlowExchange::new(sim.clone(), clock.clone(), Duration::zero())
            .with_slow_queries(Duration::seconds(10)),
    );
    let runtime = Runtime::with_clock(slow, store, RuntimeOptions::default(), clock.clone());
    let strategy_id = runtime
        .create_strategy(
            throttled(long_grid_config(), Duration::hours(1)),
            btc_spec(),
            btc_fees(),
        )
        .unwrap();

    // 限速之下只有第 0 格挂单。
    let report = runtime.step(&strategy_id).await.unwrap();
    assert_eq!(report.placed.len(), 1);
    let cloid = report.placed[0].clone();

    // 这一步开始之后、查到订单之前成交了 .02，成交查询有延迟，这一步还看不到它；
    // 订单随即被撤销：交易所那边是「撤销、成交 .02」，本地还以为它挂着。
    sim.delay_next_fills(1);
    sim.fill(&cloid, Quantity::parse("0.02").unwrap(), at(95))
        .unwrap();
    sim.cancel_order(&btc(), &cloid).await.unwrap();

    // 这一步从 t = 90 开始，查到订单状态时已经是 t = 100。
    clock.set(at(90));
    runtime.step(&strategy_id).await.unwrap();

    let state = runtime.state(&strategy_id).unwrap();
    assert_eq!(
        long_state(&state).throttle().first_fill_at,
        Some(at(100)),
        "成交时刻取拿到订单状态的时刻，不是这一步开始的 at(90)"
    );

    // 真实流水到了之后照常校正为实际成交时刻。
    sim.deliver_delayed_fills();
    clock.set(at(200));
    runtime.step(&strategy_id).await.unwrap();

    let state = runtime.state(&strategy_id).unwrap();
    assert_eq!(long_state(&state).throttle().first_fill_at, Some(at(95)));
}
