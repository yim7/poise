//! 第三方市场（HIP-3）上的标的，例如黄金 `xyz:GOLD`：和主市场一样走新建、预览、确认启动和
//! 运行；账户保证金率要把它所在的市场算进去（第 7.4 节）。

mod common;

use std::sync::Arc;

use common::{at, btc_fees, long_grid_config, start_strategy};
use poise_runtime::{
    ManualClock, Runtime, RuntimeOptions, SimulatedAccount, SimulatedExchange, Store,
};
use poise_strategy::instrument::{Instrument, InstrumentSpec};
use poise_strategy::units::{Notional, Price, Quantity};

fn gold() -> Instrument {
    Instrument::try_new("xyz:GOLD").unwrap()
}

/// 黄金按 4183.9 美元报价、数量精度 0.0001（正式网 `xyz:GOLD` 的 `szDecimals` 是 4）。
fn gold_spec() -> InstrumentSpec {
    InstrumentSpec::new(
        gold(),
        Price::parse("0.1").unwrap(),
        Quantity::parse("0.0001").unwrap(),
        Notional::parse("10").unwrap(),
    )
    .unwrap()
}

/// 黄金上的做多网格：4000 到 4200，最多 0.1 盎司，间距 20。
fn gold_config() -> poise_strategy::config::StrategyConfig {
    let mut config = long_grid_config();
    config.instrument = gold();
    let side = config.long.as_mut().unwrap();
    side.price_low = Price::parse("4000").unwrap();
    side.price_high = Price::parse("4200").unwrap();
    side.max_position = Quantity::parse("0.1").unwrap();
    side.grid_spacing = Price::parse("20").unwrap();
    side.grid_spread = Price::parse("20").unwrap();
    config
}

/// 第三方市场的标的照常启动、挂单；走一步时，账户保证金读数把它所在的市场算进去。
#[tokio::test]
async fn a_hip3_strategy_runs_and_its_dex_counts_toward_the_margin() {
    let exchange = Arc::new(SimulatedExchange::new(
        gold(),
        Price::parse("4183.9").unwrap(),
        at(0),
    ));
    let account = Arc::new(SimulatedAccount::new(gold_spec(), btc_fees()));
    let clock = Arc::new(ManualClock::new(at(0)));
    let runtime = Runtime::with_clock(
        exchange.clone(),
        account.clone(),
        Store::open_in_memory().unwrap(),
        RuntimeOptions::default(),
        clock,
    );
    let strategy_id = start_strategy(&runtime, gold_config()).await;

    let report = runtime.step(&strategy_id).await.unwrap();

    assert!(!report.placed.is_empty());
    assert!(
        exchange
            .orders()
            .iter()
            .all(|order| order.client_order_id.is_some())
    );
    assert_eq!(account.margin_instruments(), vec![gold()]);
}
