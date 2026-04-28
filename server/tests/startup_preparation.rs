use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use poise_application::{ConfiguredTrackDefinition, ConfiguredTrackInput, PreparedTrackRegistry};
use poise_core::strategy::ShapeFamily;
use poise_core::track::{Instrument, TrackId, Venue};
use poise_core::types::ExchangeRules;
use poise_engine::ports::{ExchangeInfo, MetadataPort};

#[path = "../src/startup_preparation.rs"]
mod startup_preparation;

use startup_preparation::{SymbolLeverageSetter, TrackLeverageIndex};

#[derive(Clone, Debug, PartialEq, Eq)]
struct FakeBuiltExchange(&'static str);

#[tokio::test]
async fn prepare_exchange_startup_builds_exchange_before_setting_leverage() {
    let call_log = Arc::new(Mutex::new(Vec::new()));
    let prepared_registry = prepared_registry("btc-core", "BTCUSDT");
    let track_leverage_index = TrackLeverageIndex::from([(TrackId::new("btc-core"), 20)]);

    let built_exchange: FakeBuiltExchange = startup_preparation::prepare_exchange_startup_with(
        &prepared_registry,
        &track_leverage_index,
        {
            let call_log = call_log.clone();
            move || {
                let call_log = call_log.clone();
                async move {
                    call_log.lock().unwrap().push("build_exchange".to_string());
                    Ok(FakeBuiltExchange("binance-startup"))
                }
            }
        },
        {
            let call_log = call_log.clone();
            move || {
                Ok(
                    Arc::new(RecordingSymbolLeverageSetter::succeed(call_log.clone()))
                        as Arc<dyn SymbolLeverageSetter>,
                )
            }
        },
    )
    .await
    .unwrap();

    assert_eq!(built_exchange, FakeBuiltExchange("binance-startup"));

    assert_eq!(
        *call_log.lock().unwrap(),
        vec![
            "build_exchange".to_string(),
            "set_leverage:BTCUSDT:20".to_string()
        ]
    );
}

#[tokio::test]
async fn prepare_exchange_startup_failure_surfaces_track_symbol_and_leverage_context() {
    let call_log = Arc::new(Mutex::new(Vec::new()));
    let prepared_registry = prepared_registry("btc-core", "BTCUSDT");
    let track_leverage_index = TrackLeverageIndex::from([(TrackId::new("btc-core"), 7)]);

    let result: Result<FakeBuiltExchange> = startup_preparation::prepare_exchange_startup_with(
        &prepared_registry,
        &track_leverage_index,
        {
            let call_log = call_log.clone();
            move || {
                let call_log = call_log.clone();
                async move {
                    call_log.lock().unwrap().push("build_exchange".to_string());
                    Ok(FakeBuiltExchange("binance-startup"))
                }
            }
        },
        {
            let call_log = call_log.clone();
            move || {
                Ok(Arc::new(RecordingSymbolLeverageSetter::fail(
                    call_log.clone(),
                    "exchange rejected leverage",
                )) as Arc<dyn SymbolLeverageSetter>)
            }
        },
    )
    .await;

    let error = match result {
        Ok(_) => panic!("prepare_exchange_startup_with should fail on leverage error"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(message.contains("btc-core"));
    assert!(message.contains("BTCUSDT"));
    assert!(message.contains("7"));
    assert!(message.contains("exchange rejected leverage"));
    assert_eq!(
        *call_log.lock().unwrap(),
        vec![
            "build_exchange".to_string(),
            "set_leverage:BTCUSDT:7".to_string()
        ]
    );
}

#[tokio::test]
async fn load_exchange_info_with_retry_retries_transient_failures() {
    let metadata = FlakyExchangeInfoPort::new(2);

    let info = startup_preparation::load_exchange_info_with_retry(
        &metadata,
        &Instrument::new(Venue::Binance, "BTCUSDT"),
    )
    .await
    .unwrap();

    assert_eq!(metadata.calls(), 3);
    assert_eq!(info.rules, test_exchange_rules());
}

fn prepared_registry(track_id: &str, symbol: &str) -> PreparedTrackRegistry {
    PreparedTrackRegistry::new(vec![
        ConfiguredTrackDefinition::try_from_input(ConfiguredTrackInput {
            track_id: TrackId::new(track_id),
            venue: Venue::Binance,
            symbol: symbol.to_string(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: Some(0.5),
            shape_family: Some(ShapeFamily::Linear),
            out_of_band_policy: None,
            max_notional: Some(3_000.0),
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: None,
        })
        .unwrap(),
    ])
    .unwrap()
}

fn test_exchange_rules() -> ExchangeRules {
    ExchangeRules {
        price_tick: 0.1,
        quantity_step: 0.1,
        min_qty: 0.0,
        min_notional: 0.0,
        maker_fee_rate: 0.0,
        taker_fee_rate: 0.0,
    }
}

struct RecordingSymbolLeverageSetter {
    calls: Arc<Mutex<Vec<String>>>,
    failure: Option<String>,
}

impl RecordingSymbolLeverageSetter {
    fn succeed(calls: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            calls,
            failure: None,
        }
    }

    fn fail(calls: Arc<Mutex<Vec<String>>>, message: impl Into<String>) -> Self {
        Self {
            calls,
            failure: Some(message.into()),
        }
    }
}

#[async_trait]
impl SymbolLeverageSetter for RecordingSymbolLeverageSetter {
    async fn set_leverage(&self, instrument: &Instrument, leverage: u32) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("set_leverage:{}:{leverage}", instrument.symbol));
        if let Some(message) = &self.failure {
            return Err(anyhow!(message.clone()));
        }
        Ok(())
    }
}

struct FlakyExchangeInfoPort {
    remaining_failures: Mutex<usize>,
    calls: Mutex<usize>,
}

impl FlakyExchangeInfoPort {
    fn new(remaining_failures: usize) -> Self {
        Self {
            remaining_failures: Mutex::new(remaining_failures),
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl MetadataPort for FlakyExchangeInfoPort {
    async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
        *self.calls.lock().unwrap() += 1;
        let mut remaining_failures = self.remaining_failures.lock().unwrap();
        if *remaining_failures > 0 {
            *remaining_failures -= 1;
            return Err(anyhow!("temporary metadata failure"));
        }
        Ok(ExchangeInfo {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            rules: test_exchange_rules(),
        })
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        Ok(Utc::now())
    }
}
