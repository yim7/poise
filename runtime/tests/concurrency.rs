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
async fn changing_the_config_waits_for_the_step_in_flight() {
    let (runtime, clock) = common::slow_runtime(std::time::Duration::from_millis(300));
    let strategy_id = start_strategy(&runtime, long_grid_config()).await;
    clock.set(at(1));

    // 这一步停在标记价上，驱动权还握在它手里。
    let stepping = tokio::spawn({
        let runtime = runtime.clone();
        let strategy_id = strategy_id.clone();
        async move { runtime.step(&strategy_id).await }
    });
    tokio::task::yield_now().await;

    let updating = tokio::spawn({
        let runtime = runtime.clone();
        let strategy_id = strategy_id.clone();
        async move {
            runtime
                .update_config(
                    &strategy_id,
                    throttled(long_grid_config(), Duration::hours(1)),
                )
                .await
        }
    });
    // 这一步没走完之前，改配置在排队，没有动库里的配置。
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!updating.is_finished(), "改配置该等这一步走完");
    assert_eq!(runtime.config_versions(&strategy_id).unwrap().len(), 1);

    stepping.await.unwrap().unwrap();
    assert_eq!(updating.await.unwrap().unwrap(), 2);
}

/// 一步走得比节拍长时，常驻服务的下一步紧接着就开始，驱动权几乎一直被占着。用户点的
/// 停止并平仓要排上队：这一步一走完就轮到它，而不是被下一步抢走、报「忙」。
#[tokio::test]
async fn stopping_while_steps_run_back_to_back_takes_effect() {
    let (runtime, clock) = common::slow_runtime(std::time::Duration::from_millis(100));
    let strategy_id = start_strategy(&runtime, long_grid_config()).await;
    clock.set(at(1));

    // 模仿常驻服务：一步接一步地走，中间不留空档；忙就是用户操作在用驱动权，跳过。
    let driving = tokio::spawn({
        let runtime = runtime.clone();
        let strategy_id = strategy_id.clone();
        async move {
            let mut skipped = 0;
            loop {
                match runtime.step(&strategy_id).await {
                    Ok(report) if report.status.is_archived() => return skipped,
                    Ok(_) => {}
                    Err(RuntimeError::Busy(_)) => {
                        skipped += 1;
                        tokio::task::yield_now().await;
                    }
                    Err(RuntimeError::Archived(_)) => return skipped,
                    Err(error) => panic!("step failed: {error:?}"),
                }
            }
        }
    });
    // 等这一步确实在走（停在标记价上）再点停止。
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.stop_and_close(&strategy_id),
    )
    .await
    .expect("停止该排上队，而不是一直等")
    .expect("停止该被接受");

    let skipped = tokio::time::timeout(std::time::Duration::from_secs(10), driving)
        .await
        .expect("停止之后常驻服务该把它收尾归档")
        .unwrap();
    assert!(skipped > 0, "停止拿着驱动权时常驻服务该跳过");
    let status = runtime.store().load_strategy(&strategy_id).unwrap().status;
    assert!(status.is_archived(), "{status:?}");
}

/// 交易所卡住、一步迟迟不走完时，用户操作等一阵子之后报忙，不会无限挂着。
#[tokio::test(start_paused = true)]
async fn a_user_action_gives_up_when_the_step_never_finishes() {
    let (runtime, clock) = common::slow_runtime(std::time::Duration::from_secs(3600));
    let strategy_id = start_strategy(&runtime, long_grid_config()).await;
    clock.set(at(1));

    let stuck = tokio::spawn({
        let runtime = runtime.clone();
        let strategy_id = strategy_id.clone();
        async move { runtime.step(&strategy_id).await }
    });
    tokio::task::yield_now().await;

    match runtime.pause(&strategy_id).await {
        Err(RuntimeError::Busy(busy)) => assert_eq!(busy, strategy_id),
        other => panic!("expected Busy, got {other:?}"),
    }
    stuck.abort();
}
