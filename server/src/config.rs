use std::path::Path;

use anyhow::{Context, Result};
use poise_application::AccountMonitorConfig;
use poise_binance as binance;
use poise_bybit as bybit;
use poise_core::risk::LossLimits;
use poise_core::strategy::{
    BandProtectionPolicy, DEFAULT_MIN_REBALANCE_UNITS, RiskIncreaseDelayConfig, ShapeFamily,
    TrackConfig,
};
use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
use poise_hyperliquid as hyperliquid;
use poise_okx as okx;
use serde::{Deserialize, Deserializer};

use crate::exchange_startup::build_track_leverage_index;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    pub tracks: Vec<TrackSpec>,
    pub exchange: ExchangeConfig,
    #[serde(default)]
    pub account_monitor: AccountMonitorConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackSpec {
    pub track_id: String,
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub min_rebalance_units: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_shape_family_option")]
    pub shape_family: Option<ShapeFamily>,
    pub out_of_band_policy: Option<BandProtectionPolicy>,
    pub max_notional: Option<f64>,
    pub leverage: Option<u32>,
    pub daily_loss_limit: f64,
    pub total_loss_limit: f64,
    pub tick_timeout_secs: Option<u64>,
    pub risk_increase_delay: Option<RiskIncreaseDelayConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "venue", rename_all = "snake_case")]
pub enum ExchangeConfig {
    Binance(binance::Config),
    Bybit(bybit::Config),
    Hyperliquid(hyperliquid::Config),
    Okx(okx::Config),
}

impl Default for ExchangeConfig {
    fn default() -> Self {
        Self::Binance(binance::Config::default())
    }
}

impl ExchangeConfig {
    pub fn venue(&self) -> Venue {
        match self {
            Self::Binance(_) => Venue::Binance,
            Self::Bybit(_) => Venue::Bybit,
            Self::Hyperliquid(_) => Venue::Hyperliquid,
            Self::Okx(_) => Venue::Okx,
        }
    }
}

pub fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file `{}`", path.display()))?;
    parse_config(&raw)
}

pub fn parse_config(input: &str) -> Result<Config> {
    let config: Config = toml_edit::de::from_str(input).context("failed to parse TOML config")?;
    for track in &config.tracks {
        track
            .to_track_definition(config.exchange.venue())
            .map_err(|error| anyhow::anyhow!("invalid track `{}`: {error}", track.track_id))?;
    }
    build_track_leverage_index(&config.tracks)?;
    config.account_monitor.validate()?;
    Ok(config)
}

impl TrackSpec {
    pub fn track_id(&self) -> TrackId {
        TrackId::new(self.track_id.clone())
    }

    pub fn to_track_definition(&self, venue: Venue) -> Result<TrackDefinition> {
        let track_config = TrackConfig {
            lower_price: self.lower_price,
            upper_price: self.upper_price,
            long_exposure_units: self.long_exposure_units,
            short_exposure_units: self.short_exposure_units,
            notional_per_unit: self.notional_per_unit,
            min_rebalance_units: self
                .min_rebalance_units
                .unwrap_or(DEFAULT_MIN_REBALANCE_UNITS),
            shape_family: self.shape_family.unwrap_or(ShapeFamily::Linear),
            out_of_band_policy: self
                .out_of_band_policy
                .unwrap_or(BandProtectionPolicy::Freeze),
            risk_increase_delay: self.risk_increase_delay,
        };
        let loss_limits = LossLimits {
            daily_loss_limit: self.daily_loss_limit,
            total_loss_limit: self.total_loss_limit,
        };

        TrackDefinition::try_new(
            self.track_id(),
            Instrument::new(venue, self.symbol.clone()),
            track_config,
            self.max_notional,
            loss_limits,
            self.tick_timeout_secs,
        )
        .map_err(anyhow::Error::msg)
    }
}

fn default_bind_address() -> String {
    "127.0.0.1:8000".to_string()
}

fn deserialize_shape_family_option<'de, D>(deserializer: D) -> Result<Option<ShapeFamily>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref() {
        None => Ok(None),
        Some("linear") => Ok(Some(ShapeFamily::Linear)),
        Some("inertial") => Ok(Some(ShapeFamily::Inertial)),
        Some("responsive") => Ok(Some(ShapeFamily::Responsive)),
        Some("concave") => Err(serde::de::Error::custom(
            "shape_family `concave` has been renamed to `inertial`",
        )),
        Some("convex") => Err(serde::de::Error::custom(
            "shape_family `convex` has been renamed to `responsive`",
        )),
        Some(other) => Err(serde::de::Error::custom(format!(
            "unknown shape_family `{other}`; expected one of: linear, inertial, responsive"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::ShapeFamily;
    use poise_core::track::{TrackId, Venue};

    use super::{
        AccountMonitorConfig, ExchangeConfig, default_bind_address, load_config, parse_config,
    };

    #[test]
    fn track_spec_builds_track_definition_from_service_venue() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        let definition = config.tracks[0]
            .to_track_definition(config.exchange.venue())
            .unwrap();
        assert_eq!(definition.track_id().as_str(), "btc-core");
        assert_eq!(definition.instrument().venue, Venue::Binance);
        assert_eq!(definition.instrument().symbol, "BTCUSDT");
        assert_eq!(definition.track_config().min_rebalance_units, 0.5);
        assert_eq!(definition.tick_timeout_secs(), 30);
    }

    #[test]
    fn parses_explicit_track_leverage() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 375.0
leverage = 20
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        assert_eq!(config.tracks[0].leverage, Some(20));
    }

    #[test]
    fn parses_risk_increase_delay_config() {
        let config = parse_config(
            r#"
bind_address = "127.0.0.1:8000"

[exchange]
venue = "binance"
deployment = "testnet"
api_key = ""
api_secret = ""

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 100.0
min_rebalance_units = 0.5
shape_family = "linear"
out_of_band_policy = "freeze"
max_notional = 800.0
leverage = 10
daily_loss_limit = 100.0
total_loss_limit = 200.0
tick_timeout_secs = 30

[tracks.risk_increase_delay]
startup_initial_ratio = 0.3
advantage_min_rebalance_multiples = 2.0
base_step_min_rebalance_multiples = 1.0
max_step_min_rebalance_multiples = 4.0
catchup_ratio = 0.25
"#,
        )
        .unwrap();

        let delay = config.tracks[0]
            .to_track_definition(config.exchange.venue())
            .unwrap()
            .track_config()
            .risk_increase_delay
            .unwrap();

        assert_eq!(delay.startup_initial_ratio, 0.3);
        assert_eq!(delay.advantage_min_rebalance_multiples, 2.0);
        assert_eq!(delay.base_step_min_rebalance_multiples, 1.0);
        assert_eq!(delay.max_step_min_rebalance_multiples, 4.0);
        assert_eq!(delay.catchup_ratio, 0.25);
    }

    #[test]
    fn parses_hyperliquid_exchange_config() {
        let config = parse_config(
            r#"
[exchange]
venue = "hyperliquid"
deployment = "testnet"
private_key = "0x1111111111111111111111111111111111111111111111111111111111111111"
wallet_address = "0x2222222222222222222222222222222222222222"

[[tracks]]
track_id = "btc-core"
symbol = "BTC"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        assert_eq!(config.exchange.venue(), Venue::Hyperliquid);
        let definition = config.tracks[0]
            .to_track_definition(config.exchange.venue())
            .unwrap();
        assert_eq!(definition.instrument().venue, Venue::Hyperliquid);
        assert_eq!(definition.instrument().symbol, "BTC");
    }

    #[test]
    fn parses_okx_exchange_config() {
        let config = parse_config(
            r#"
[exchange]
venue = "okx"
deployment = "demo"
api_key = "demo-key"
api_secret = "demo-secret"
passphrase = "demo-passphrase"

[[tracks]]
track_id = "btc-core"
symbol = "BTC-USDT-SWAP"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        assert_eq!(config.exchange.venue(), Venue::Okx);
        let definition = config.tracks[0]
            .to_track_definition(config.exchange.venue())
            .unwrap();
        assert_eq!(definition.instrument().venue, Venue::Okx);
        assert_eq!(definition.instrument().symbol, "BTC-USDT-SWAP");
        if let ExchangeConfig::Okx(exchange) = &config.exchange {
            assert_eq!(exchange.deployment, poise_okx::Deployment::Demo);
            assert_eq!(exchange.api_key.as_deref(), Some("demo-key"));
            assert_eq!(exchange.api_secret.as_deref(), Some("demo-secret"));
            assert_eq!(exchange.passphrase.as_deref(), Some("demo-passphrase"));
        } else {
            panic!("expected OKX fixture to parse as ExchangeConfig::Okx");
        }
    }

    #[test]
    fn rejects_zero_leverage_at_config_boundary() {
        let error = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 375.0
leverage = 0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("leverage"));
    }

    #[test]
    fn config_module_examples_do_not_define_environment() {
        let source = include_str!("config.rs");
        let environment_field_signature = ["pub", " environment", ":"].concat();

        assert!(!source.contains("environment = \"paper\""));
        assert!(!source.contains(&environment_field_signature));
    }

    #[test]
    fn exchange_config_does_not_expose_direct_credential_accessors() {
        let source = include_str!("config.rs");
        let api_key_signature = ["pub", " fn", " api_key", "("].concat();
        let api_secret_signature = ["pub", " fn", " api_secret", "("].concat();

        assert!(!source.contains(&api_key_signature));
        assert!(!source.contains(&api_secret_signature));
    }

    #[test]
    fn parses_binance_exchange_config_and_tracks() {
        let config = parse_config(
            r#"
bind_address = "127.0.0.1:9000"

[exchange]
venue = "binance"
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 1200.0
total_loss_limit = 2400.0

[[tracks]]
track_id = "eth-core"
symbol = "ETHUSDT"
lower_price = 2000.0
upper_price = 2600.0
long_exposure_units = 5.0
short_exposure_units = 4.0
notional_per_unit = 2000.0
daily_loss_limit = 800.0
total_loss_limit = 1600.0
shape_family = "inertial"
out_of_band_policy = "freeze"
"#,
        )
        .unwrap();

        assert_eq!(config.bind_address, "127.0.0.1:9000");
        assert_eq!(config.tracks.len(), 2);
        assert_eq!(config.tracks[0].symbol, "BTCUSDT");
        assert_eq!(config.tracks[0].track_id().as_str(), "btc-core");
        assert_eq!(
            config.tracks[1].shape_family,
            Some(poise_core::strategy::ShapeFamily::Inertial)
        );
        assert_eq!(
            config.tracks[1].out_of_band_policy,
            Some(poise_core::strategy::BandProtectionPolicy::Freeze)
        );
        if let ExchangeConfig::Binance(exchange) = &config.exchange {
            assert_eq!(exchange.api_key.as_deref(), Some("demo-key"));
            assert_eq!(exchange.api_secret.as_deref(), Some("demo-secret"));
        } else {
            panic!("expected Binance fixture to parse as ExchangeConfig::Binance");
        }
    }

    #[test]
    fn config_toml_parses_flatten_trigger_and_reentry_confirm_policy() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 75000
upper_price = 80800
long_exposure_units = 8
short_exposure_units = 8
notional_per_unit = 375
daily_loss_limit = 120
total_loss_limit = 500
out_of_band_policy = { flatten = { trigger = { flatten_confirm = { bps = 500 } }, recover = { reentry_confirm = { bps = 500 } } } }
"#,
        )
        .unwrap();

        assert!(matches!(
            config.tracks[0].out_of_band_policy,
            Some(poise_core::strategy::BandProtectionPolicy::Flatten {
                trigger: poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: poise_core::strategy::BandRecoverPolicy::ReentryConfirm { bps: 500 }
            })
        ));
    }

    #[test]
    fn config_toml_parses_flatten_shorthand_as_current_default() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 75000
upper_price = 80800
long_exposure_units = 8
short_exposure_units = 8
notional_per_unit = 375
daily_loss_limit = 120
total_loss_limit = 500
out_of_band_policy = "flatten"
"#,
        )
        .unwrap();

        assert!(matches!(
            config.tracks[0].out_of_band_policy,
            Some(poise_core::strategy::BandProtectionPolicy::Flatten {
                trigger: poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: poise_core::strategy::BandRecoverPolicy::ReentryConfirm { bps: 500 }
            })
        ));
    }

    #[test]
    fn parses_service_level_exchange_config_and_track_symbols() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"
deployment = "testnet"
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
daily_loss_limit = 1200.0
total_loss_limit = 2400.0
"#,
        )
        .unwrap();

        assert_eq!(config.exchange.venue(), Venue::Binance);
        assert_eq!(config.tracks[0].symbol, "BTCUSDT");
    }

    #[test]
    fn parses_bybit_exchange_config_and_tracks() {
        let config = parse_config(
            r#"
[exchange]
venue = "bybit"
deployment = "testnet"
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        assert_eq!(config.exchange.venue(), Venue::Bybit);
        assert_eq!(config.tracks[0].symbol, "BTCUSDT");
        if let ExchangeConfig::Bybit(exchange) = &config.exchange {
            assert_eq!(exchange.deployment, poise_bybit::Deployment::Testnet);
            assert_eq!(exchange.api_key.as_deref(), Some("demo-key"));
            assert_eq!(exchange.api_secret.as_deref(), Some("demo-secret"));
        } else {
            panic!("expected Bybit fixture to parse as ExchangeConfig::Bybit");
        }
    }

    #[test]
    fn parses_demo_config() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../configs/demo.toml");
        let config = load_config(&path).unwrap();

        assert_eq!(config.exchange.venue(), Venue::Binance);
        if let ExchangeConfig::Binance(exchange) = &config.exchange {
            assert_eq!(exchange.deployment, poise_binance::Deployment::Testnet);
        } else {
            panic!("expected demo config to parse as ExchangeConfig::Binance");
        }
        assert_eq!(config.tracks.len(), 1);
        assert_eq!(config.tracks[0].track_id(), TrackId::new("btc-core"));
        assert_eq!(config.tracks[0].leverage, Some(10));
    }

    #[test]
    fn rejects_legacy_track_level_venue_field() {
        let error = parse_config(
            r#"
[exchange]
venue = "binance"
deployment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0
"#,
        )
        .unwrap_err();

        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("unknown field `venue`"))
        );
    }

    #[test]
    fn defaults_bind_address_and_exchange_credentials_for_testnet() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        let track = config.tracks[0]
            .to_track_definition(config.exchange.venue())
            .unwrap();

        assert_eq!(config.bind_address, default_bind_address());
        if let ExchangeConfig::Binance(exchange) = &config.exchange {
            assert_eq!(exchange.api_key, None);
            assert_eq!(exchange.api_secret, None);
        } else {
            panic!("expected default exchange to remain Binance");
        }
        assert_eq!(
            track.track_config().shape_family,
            poise_core::strategy::ShapeFamily::Linear
        );
        assert_eq!(
            track.track_config().out_of_band_policy,
            poise_core::strategy::BandProtectionPolicy::Freeze
        );
        assert!((track.track_config().min_rebalance_units - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn defaults_min_rebalance_units_to_point_five() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        let track = config.tracks[0]
            .to_track_definition(config.exchange.venue())
            .unwrap();

        assert!((track.track_config().min_rebalance_units - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_negative_min_rebalance_units_at_config_boundary() {
        let error = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
min_rebalance_units = -0.1
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("min_rebalance_units"));
    }

    #[test]
    fn rejects_non_finite_min_rebalance_units_at_config_boundary() {
        let error = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
min_rebalance_units = nan
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("min_rebalance_units"));
    }

    #[test]
    fn parses_optional_tick_timeout_secs() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
tick_timeout_secs = 45
"#,
        )
        .unwrap();

        assert_eq!(config.tracks[0].tick_timeout_secs, Some(45));
    }

    #[test]
    fn parses_new_shape_family_names() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 4.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
shape_family = "inertial"
"#,
        )
        .unwrap();

        let track = &config.tracks[0];
        assert_eq!(track.shape_family, Some(ShapeFamily::Inertial));
    }

    #[test]
    fn rejects_legacy_shape_family_names_with_migration_hint() {
        let error = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 4.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
shape_family = "concave"
"#,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("concave"));
        assert!(format!("{error:#}").contains("inertial"));
    }

    #[test]
    fn track_definition_uses_flat_max_notional_and_loss_limits_when_configured() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
max_notional = 5000.0
daily_loss_limit = 200.0
total_loss_limit = 500.0
"#,
        )
        .unwrap();

        let definition = config.tracks[0]
            .to_track_definition(config.exchange.venue())
            .unwrap();
        assert!((definition.max_notional() - 5000.0).abs() < f64::EPSILON);
        assert!((definition.loss_limits().daily_loss_limit - 200.0).abs() < f64::EPSILON);
        assert!((definition.loss_limits().total_loss_limit - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_missing_risk_limits() {
        let error = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("daily_loss_limit"));
    }

    #[test]
    fn parses_explicit_track_id_from_config_instead_of_deriving_from_symbol() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        assert_eq!(config.tracks[0].symbol, "BTCUSDT");
        assert_eq!(config.tracks[0].track_id().as_str(), "btc-core");
    }

    #[test]
    fn demo_config_has_current_track_shape() {
        let config = parse_config(include_str!("../../configs/demo.toml")).unwrap();
        let track = &config.tracks[0];
        let equivalent_track_step = (track.upper_price - track.lower_price)
            / (track.long_exposure_units + track.short_exposure_units);

        assert_eq!(config.tracks.len(), 1);
        assert_eq!(track.track_id().as_str(), "btc-core");
        assert_eq!(track.upper_price - track.lower_price, 2000.0);
        assert!((equivalent_track_step - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn demo_config_explicitly_lists_complete_track_parameters() {
        let raw = include_str!("../../configs/demo.toml");

        assert!(raw.contains("min_rebalance_units ="));
        assert!(raw.contains("shape_family ="));
        assert!(raw.contains("out_of_band_policy ="));
        assert!(raw.contains("tick_timeout_secs ="));
        assert!(raw.contains("max_notional ="));
        assert!(raw.contains("daily_loss_limit ="));
        assert!(raw.contains("total_loss_limit ="));
    }

    #[test]
    fn demo_config_defines_exchange_only_at_service_level() {
        let raw = include_str!("../../configs/demo.toml");

        assert_eq!(raw.matches("venue = ").count(), 1);
        assert!(!raw.contains("[[tracks]]\ntrack_id = \"btc-core\"\nvenue = "));
    }

    #[test]
    fn readme_example_matches_service_level_exchange_boundary() {
        let raw = include_str!("../../README.md");

        assert!(raw.contains("[exchange]\nvenue = \"binance\"\ndeployment = \"testnet\""));
        assert!(raw.contains("configs/demo.toml"));
        assert!(!raw.contains("[[tracks]]\ntrack_id = \"btc-core\"\nvenue = "));
        assert!(!raw.contains("environment = \"testnet\" 时，服务端固定接 Binance"));
        assert!(!raw.contains("environment = \"mainnet\" 时，服务端固定接 Binance"));
        assert!(!raw.contains("environment = \"testnet\""));
        assert!(!raw.contains("environment = \"mainnet\""));
        assert!(!raw.contains("environment = \"test\""));
    }

    #[test]
    fn parses_config_without_environment_field() {
        let config = parse_config(
            r#"
bind_address = "127.0.0.1:8000"

[exchange]
venue = "binance"
deployment = "testnet"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        assert_eq!(config.bind_address, "127.0.0.1:8000");
    }

    #[test]
    fn demo_config_does_not_define_environment() {
        let raw = include_str!("../../configs/demo.toml");

        assert!(!raw.contains("environment = "));
    }

    #[test]
    fn defaults_account_monitor_thresholds() {
        let config = parse_config(
            r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap();

        assert_eq!(config.account_monitor, AccountMonitorConfig::default());
    }

    #[test]
    fn rejects_inverted_account_monitor_thresholds() {
        let error = parse_config(
            r#"
[exchange]
venue = "binance"

[account_monitor]
day_change_attention_pct = -6.0
day_change_critical_pct = -5.0

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("day_change_attention_pct"));
    }
}
