use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use poise_application::TrackDefinitionRegistry;
use poise_core::risk::LossLimits;
use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
use poise_core::types::ExchangeRules;
use poise_engine::ports::{ExchangeInfo, MetadataPort};

#[path = "../src/startup_preparation.rs"]
mod startup_preparation;

use startup_preparation::{ExchangeStartupControl, TrackLeverageIndex};

#[derive(Clone, Debug, PartialEq, Eq)]
struct FakeBuiltExchange(&'static str);

#[tokio::test]
async fn prepare_exchange_startup_builds_exchange_before_setting_leverage() {
    let call_log = Arc::new(Mutex::new(Vec::new()));
    let track_definition_registry = track_definition_registry("btc-core", "BTCUSDT");
    let track_leverage_index = TrackLeverageIndex::from([(TrackId::new("btc-core"), 20)]);

    let built_exchange: FakeBuiltExchange = startup_preparation::prepare_exchange_startup_with(
        &track_definition_registry,
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
                    Arc::new(RecordingExchangeStartupControl::succeed(call_log.clone()))
                        as Arc<dyn ExchangeStartupControl>,
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
            "validate_account_mode".to_string(),
            "validate_instrument_mode:BTCUSDT".to_string(),
            "set_leverage:BTCUSDT:20".to_string()
        ]
    );
}

#[tokio::test]
async fn prepare_exchange_startup_failure_surfaces_track_symbol_and_leverage_context() {
    let call_log = Arc::new(Mutex::new(Vec::new()));
    let track_definition_registry = track_definition_registry("btc-core", "BTCUSDT");
    let track_leverage_index = TrackLeverageIndex::from([(TrackId::new("btc-core"), 7)]);

    let result: Result<FakeBuiltExchange> = startup_preparation::prepare_exchange_startup_with(
        &track_definition_registry,
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
                Ok(Arc::new(RecordingExchangeStartupControl::fail(
                    call_log.clone(),
                    "exchange rejected leverage",
                )) as Arc<dyn ExchangeStartupControl>)
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
            "validate_account_mode".to_string(),
            "validate_instrument_mode:BTCUSDT".to_string(),
            "set_leverage:BTCUSDT:7".to_string()
        ]
    );
}

#[tokio::test]
async fn prepare_exchange_startup_stops_before_leverage_when_account_mode_validation_fails() {
    let call_log = Arc::new(Mutex::new(Vec::new()));
    let track_definition_registry = track_definition_registry("btc-core", "BTCUSDT");
    let track_leverage_index = TrackLeverageIndex::from([(TrackId::new("btc-core"), 20)]);

    let result: Result<FakeBuiltExchange> = startup_preparation::prepare_exchange_startup_with(
        &track_definition_registry,
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
                Ok(Arc::new(RecordingExchangeStartupControl::fail_validation(
                    call_log.clone(),
                    "wrong position mode",
                )) as Arc<dyn ExchangeStartupControl>)
            }
        },
    )
    .await;

    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("failed to validate exchange account mode"));
    assert!(error.contains("wrong position mode"));
    assert_eq!(
        *call_log.lock().unwrap(),
        vec![
            "build_exchange".to_string(),
            "validate_account_mode".to_string()
        ]
    );
}

#[tokio::test]
async fn prepare_exchange_startup_stops_before_leverage_when_instrument_mode_validation_fails() {
    let call_log = Arc::new(Mutex::new(Vec::new()));
    let track_definition_registry = track_definition_registry("btc-core", "BTCUSDT");
    let track_leverage_index = TrackLeverageIndex::from([(TrackId::new("btc-core"), 20)]);

    let result: Result<FakeBuiltExchange> = startup_preparation::prepare_exchange_startup_with(
        &track_definition_registry,
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
                    Arc::new(RecordingExchangeStartupControl::fail_instrument_validation(
                        call_log.clone(),
                        "wrong symbol mode",
                    )) as Arc<dyn ExchangeStartupControl>,
                )
            }
        },
    )
    .await;

    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("failed to validate startup mode"));
    assert!(error.contains("btc-core"));
    assert!(error.contains("BTCUSDT"));
    assert!(error.contains("wrong symbol mode"));
    assert_eq!(
        *call_log.lock().unwrap(),
        vec![
            "build_exchange".to_string(),
            "validate_account_mode".to_string(),
            "validate_instrument_mode:BTCUSDT".to_string()
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

fn track_definition_registry(track_id: &str, symbol: &str) -> TrackDefinitionRegistry {
    TrackDefinitionRegistry::new(vec![
        TrackDefinition::try_new(
            TrackId::new(track_id),
            Instrument::new(Venue::Binance, symbol.to_string()),
            TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 6.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: BandProtectionPolicy::Freeze,
                risk_acquisition: Default::default(),
            },
            Some(3_000.0),
            LossLimits {
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
            },
            None,
        )
        .unwrap(),
    ])
    .unwrap()
}

fn test_exchange_rules() -> ExchangeRules {
    ExchangeRules {
        price_tick: 0.1,
        price_precision: Default::default(),
        quantity_kind: Default::default(),
        contract_notional: None,
        settlement_asset: "USDT".to_string(),
        quantity_step: 0.1,
        min_qty: 0.0,
        min_notional: 0.0,
        maker_fee_rate: 0.0,
        taker_fee_rate: 0.0,
    }
}

struct RecordingExchangeStartupControl {
    calls: Arc<Mutex<Vec<String>>>,
    validation_failure: Option<String>,
    instrument_validation_failure: Option<String>,
    failure: Option<String>,
}

impl RecordingExchangeStartupControl {
    fn succeed(calls: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            calls,
            validation_failure: None,
            instrument_validation_failure: None,
            failure: None,
        }
    }

    fn fail(calls: Arc<Mutex<Vec<String>>>, message: impl Into<String>) -> Self {
        Self {
            calls,
            validation_failure: None,
            instrument_validation_failure: None,
            failure: Some(message.into()),
        }
    }

    fn fail_validation(calls: Arc<Mutex<Vec<String>>>, message: impl Into<String>) -> Self {
        Self {
            calls,
            validation_failure: Some(message.into()),
            instrument_validation_failure: None,
            failure: None,
        }
    }

    fn fail_instrument_validation(
        calls: Arc<Mutex<Vec<String>>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            calls,
            validation_failure: None,
            instrument_validation_failure: Some(message.into()),
            failure: None,
        }
    }
}

#[async_trait]
impl ExchangeStartupControl for RecordingExchangeStartupControl {
    async fn validate_account_mode(&self) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push("validate_account_mode".to_string());
        if let Some(message) = &self.validation_failure {
            return Err(anyhow!(message.clone()));
        }
        Ok(())
    }

    async fn validate_instrument_mode(&self, instrument: &Instrument) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("validate_instrument_mode:{}", instrument.symbol));
        if let Some(message) = &self.instrument_validation_failure {
            return Err(anyhow!(message.clone()));
        }
        Ok(())
    }

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
