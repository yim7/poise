use std::path::Path;

use anyhow::{Context, Result, ensure};
use poise_core::risk::CapacityBudget;
use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig, validate_config};
use poise_engine::track::{Instrument, TrackId, Venue};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub environment: String,
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    pub tracks: Vec<TrackDefinition>,
    #[serde(default)]
    pub exchange: ExchangeConfig,
    #[serde(default)]
    pub account_monitor: AccountMonitorConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TrackDefinition {
    pub track_id: String,
    pub venue: Venue,
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    #[serde(default = "default_min_rebalance_units")]
    pub min_rebalance_units: f64,
    #[serde(default = "default_shape_family")]
    pub shape_family: ShapeFamily,
    #[serde(default = "default_out_of_band_policy")]
    pub out_of_band_policy: OutOfBandPolicy,
    pub max_notional: Option<f64>,
    pub daily_loss_limit: Option<f64>,
    pub stop_loss_pct: Option<f64>,
    pub tick_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ExchangeConfig {
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccountMonitorConfig {
    #[serde(default = "default_day_change_attention_pct")]
    pub day_change_attention_pct: f64,
    #[serde(default = "default_day_change_critical_pct")]
    pub day_change_critical_pct: f64,
    #[serde(default = "default_available_ratio_attention_pct")]
    pub available_ratio_attention_pct: f64,
    #[serde(default = "default_available_ratio_critical_pct")]
    pub available_ratio_critical_pct: f64,
    #[serde(default = "default_unrealized_loss_attention_pct")]
    pub unrealized_loss_attention_pct: f64,
    #[serde(default = "default_unrealized_loss_critical_pct")]
    pub unrealized_loss_critical_pct: f64,
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
        validate_config(&track.track_config())
            .map_err(|error| anyhow::anyhow!("invalid track `{}`: {error}", track.track_id))?;
    }
    config.account_monitor.validate()?;
    Ok(config)
}

impl Default for AccountMonitorConfig {
    fn default() -> Self {
        Self {
            day_change_attention_pct: default_day_change_attention_pct(),
            day_change_critical_pct: default_day_change_critical_pct(),
            available_ratio_attention_pct: default_available_ratio_attention_pct(),
            available_ratio_critical_pct: default_available_ratio_critical_pct(),
            unrealized_loss_attention_pct: default_unrealized_loss_attention_pct(),
            unrealized_loss_critical_pct: default_unrealized_loss_critical_pct(),
        }
    }
}

impl TrackDefinition {
    pub fn tick_timeout_secs(&self) -> u64 {
        self.tick_timeout_secs.unwrap_or(30)
    }

    pub fn track_id(&self) -> TrackId {
        TrackId::new(self.track_id.clone())
    }

    pub fn instrument(&self) -> Instrument {
        Instrument::new(self.venue, self.symbol.clone())
    }

    pub fn track_config(&self) -> TrackConfig {
        TrackConfig {
            lower_price: self.lower_price,
            upper_price: self.upper_price,
            long_exposure_units: self.long_exposure_units,
            short_exposure_units: self.short_exposure_units,
            notional_per_unit: self.notional_per_unit,
            min_rebalance_units: self.min_rebalance_units,
            shape_family: self.shape_family,
            out_of_band_policy: self.out_of_band_policy,
        }
    }

    pub fn budget(&self) -> CapacityBudget {
        let implied_max_notional =
            self.long_exposure_units.max(self.short_exposure_units) * self.notional_per_unit;
        CapacityBudget {
            max_notional: self.max_notional.unwrap_or(implied_max_notional),
            daily_loss_limit: self.daily_loss_limit.unwrap_or(-implied_max_notional * 0.1),
            stop_loss_pct: self.stop_loss_pct.unwrap_or(10.0),
        }
    }
}

impl AccountMonitorConfig {
    fn validate(&self) -> Result<()> {
        validate_threshold_pair(
            "day_change_attention_pct",
            self.day_change_attention_pct,
            "day_change_critical_pct",
            self.day_change_critical_pct,
        )?;
        validate_threshold_pair(
            "available_ratio_attention_pct",
            self.available_ratio_attention_pct,
            "available_ratio_critical_pct",
            self.available_ratio_critical_pct,
        )?;
        validate_threshold_pair(
            "unrealized_loss_attention_pct",
            self.unrealized_loss_attention_pct,
            "unrealized_loss_critical_pct",
            self.unrealized_loss_critical_pct,
        )?;

        Ok(())
    }
}

fn default_bind_address() -> String {
    "127.0.0.1:8000".to_string()
}

fn default_shape_family() -> ShapeFamily {
    ShapeFamily::Linear
}

fn default_out_of_band_policy() -> OutOfBandPolicy {
    OutOfBandPolicy::Freeze
}

fn default_min_rebalance_units() -> f64 {
    poise_core::strategy::DEFAULT_MIN_REBALANCE_UNITS
}

fn default_day_change_attention_pct() -> f64 {
    -3.0
}

fn default_day_change_critical_pct() -> f64 {
    -5.0
}

fn default_available_ratio_attention_pct() -> f64 {
    30.0
}

fn default_available_ratio_critical_pct() -> f64 {
    15.0
}

fn default_unrealized_loss_attention_pct() -> f64 {
    -5.0
}

fn default_unrealized_loss_critical_pct() -> f64 {
    -10.0
}

fn validate_threshold_pair(
    attention_name: &str,
    attention: f64,
    critical_name: &str,
    critical: f64,
) -> Result<()> {
    ensure!(attention.is_finite(), "{attention_name} must be finite");
    ensure!(critical.is_finite(), "{critical_name} must be finite");
    ensure!(
        attention >= critical,
        "{attention_name} must be greater than or equal to {critical_name}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily};

    use super::{AccountMonitorConfig, default_bind_address, parse_config};

    #[test]
    fn config_module_examples_do_not_use_paper_environment() {
        let source = include_str!("config.rs");

        assert!(!source.contains("environment = \"paper\""));
    }

    #[test]
    fn parses_config_file_with_tracks_and_exchange() {
        let config = parse_config(
            r#"
environment = "test"
bind_address = "127.0.0.1:9000"

[exchange]
api_key = "demo-key"
api_secret = "demo-secret"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0

[[tracks]]
track_id = "eth-core"
venue = "binance"
symbol = "ETHUSDT"
lower_price = 2000.0
upper_price = 2600.0
long_exposure_units = 5.0
short_exposure_units = 4.0
notional_per_unit = 2000.0
shape_family = "concave"
out_of_band_policy = "hold"
"#,
        )
        .unwrap();

        assert_eq!(config.environment, "test");
        assert_eq!(config.bind_address, "127.0.0.1:9000");
        assert_eq!(config.tracks.len(), 2);
        assert_eq!(config.tracks[0].symbol, "BTCUSDT");
        assert_eq!(config.tracks[0].track_id().as_str(), "btc-core");
        assert_eq!(
            config.tracks[1].shape_family,
            poise_core::strategy::ShapeFamily::Concave
        );
        assert_eq!(
            config.tracks[1].out_of_band_policy,
            poise_core::strategy::OutOfBandPolicy::Hold
        );
        assert_eq!(config.exchange.api_key.as_deref(), Some("demo-key"));
        assert_eq!(config.exchange.api_secret.as_deref(), Some("demo-secret"));
    }

    #[test]
    fn defaults_bind_address_and_exchange_credentials_for_testnet() {
        let config = parse_config(
            r#"
environment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap();

        assert_eq!(config.bind_address, default_bind_address());
        assert_eq!(config.exchange.api_key, None);
        assert_eq!(config.exchange.api_secret, None);
        assert_eq!(
            config.tracks[0].track_config().shape_family,
            poise_core::strategy::ShapeFamily::Linear
        );
        assert_eq!(
            config.tracks[0].track_config().out_of_band_policy,
            poise_core::strategy::OutOfBandPolicy::Freeze
        );
        assert!((config.tracks[0].track_config().min_rebalance_units - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn defaults_min_rebalance_units_to_point_five() {
        let config = parse_config(
            r#"
environment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap();

        assert!((config.tracks[0].track_config().min_rebalance_units - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_negative_min_rebalance_units_at_config_boundary() {
        let error = parse_config(
            r#"
environment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
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
environment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
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
environment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
tick_timeout_secs = 45
"#,
        )
        .unwrap();

        assert_eq!(config.tracks[0].tick_timeout_secs, Some(45));
    }

    #[test]
    fn parses_snake_case_strategy_enums() {
        let config = parse_config(
            r#"
environment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 4.0
notional_per_unit = 375.0
shape_family = "concave"
out_of_band_policy = "reduce_only"
"#,
        )
        .unwrap();

        let track = &config.tracks[0];
        assert_eq!(track.shape_family, ShapeFamily::Concave);
        assert_eq!(track.out_of_band_policy, OutOfBandPolicy::ReduceOnly);
        assert_eq!(track.budget().max_notional, 3000.0);
    }

    #[test]
    fn budget_uses_explicit_risk_limits_when_configured() {
        let config = parse_config(
            r#"
environment = "test"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
max_notional = 5000.0
daily_loss_limit = -200.0
stop_loss_pct = 5.0
"#,
        )
        .unwrap();

        let budget = config.tracks[0].budget();
        assert!((budget.max_notional - 5000.0).abs() < f64::EPSILON);
        assert!((budget.daily_loss_limit - (-200.0)).abs() < f64::EPSILON);
        assert!((budget.stop_loss_pct - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_uses_safe_defaults_when_risk_limits_omitted() {
        let config = parse_config(
            r#"
environment = "test"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap();

        let budget = config.tracks[0].budget();
        let implied_max = 8.0 * 375.0;
        assert!((budget.max_notional - implied_max).abs() < f64::EPSILON);
        assert!((budget.daily_loss_limit - (-implied_max * 0.1)).abs() < f64::EPSILON);
        assert!((budget.stop_loss_pct - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_explicit_track_id_from_config_instead_of_deriving_from_symbol() {
        let config = parse_config(
            r#"
environment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap();

        assert_eq!(config.tracks[0].symbol, "BTCUSDT");
        assert_eq!(config.tracks[0].track_id().as_str(), "btc-core");
    }

    #[test]
    fn parses_binance_testnet_example_config() {
        let config = parse_config(include_str!("../../configs/binance-testnet.demo.toml")).unwrap();
        let track = &config.tracks[0];
        let equivalent_track_step = (track.upper_price - track.lower_price)
            / (track.long_exposure_units + track.short_exposure_units);

        assert_eq!(config.environment, "testnet");
        assert_eq!(config.tracks.len(), 1);
        assert_eq!(track.track_id().as_str(), "btc-core");
        assert_eq!(track.upper_price - track.lower_price, 5500.0);
        assert!((equivalent_track_step - 137.5).abs() < f64::EPSILON);
    }

    #[test]
    fn binance_testnet_example_config_explicitly_lists_complete_track_parameters() {
        let raw = include_str!("../../configs/binance-testnet.demo.toml");

        assert!(raw.contains("min_rebalance_units ="));
        assert!(raw.contains("shape_family ="));
        assert!(raw.contains("out_of_band_policy ="));
        assert!(raw.contains("tick_timeout_secs ="));
        assert!(raw.contains("max_notional ="));
        assert!(raw.contains("daily_loss_limit ="));
        assert!(raw.contains("stop_loss_pct ="));
    }

    #[test]
    fn test_demo_config_explicitly_lists_complete_track_parameters() {
        let raw = include_str!("../../configs/test.demo.toml");

        assert!(raw.contains("min_rebalance_units ="));
        assert!(raw.contains("shape_family ="));
        assert!(raw.contains("out_of_band_policy ="));
        assert!(raw.contains("tick_timeout_secs ="));
        assert!(raw.contains("max_notional ="));
        assert!(raw.contains("daily_loss_limit ="));
        assert!(raw.contains("stop_loss_pct ="));
    }

    #[test]
    fn defaults_account_monitor_thresholds() {
        let config = parse_config(
            r#"
environment = "testnet"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap();

        assert_eq!(config.account_monitor, AccountMonitorConfig::default());
    }

    #[test]
    fn rejects_inverted_account_monitor_thresholds() {
        let error = parse_config(
            r#"
environment = "testnet"

[account_monitor]
day_change_attention_pct = -6.0
day_change_critical_pct = -5.0

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("day_change_attention_pct"));
    }
}
