use anyhow::{Context, Result};
use grid_core::risk::CapacityBudget;
use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
use grid_engine::grid::{GridId, Instrument, Venue};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub environment: String,
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    pub grids: Vec<GridDefinition>,
    #[serde(default)]
    pub exchange: ExchangeConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GridDefinition {
    pub grid_id: String,
    pub venue: Venue,
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    #[serde(default = "default_shape_family")]
    pub shape_family: ShapeFamily,
    #[serde(default = "default_out_of_band_policy")]
    pub out_of_band_policy: OutOfBandPolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ExchangeConfig {
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub rest_base_url: Option<String>,
    pub ws_base_url: Option<String>,
}

pub fn load_config(path: &str) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file `{path}`"))?;
    parse_config(&raw)
}

pub fn parse_config(input: &str) -> Result<Config> {
    toml_edit::de::from_str(input).context("failed to parse TOML config")
}

impl Config {
    pub fn default_db_path(&self) -> std::path::PathBuf {
        std::path::Path::new(".data")
            .join(&self.environment)
            .join("grid-server.sqlite")
    }
}

impl GridDefinition {
    pub fn grid_id(&self) -> GridId {
        GridId::new(self.grid_id.clone())
    }

    pub fn instrument(&self) -> Instrument {
        Instrument::new(self.venue, self.symbol.clone())
    }

    pub fn grid_config(&self) -> GridConfig {
        GridConfig {
            lower_price: self.lower_price,
            upper_price: self.upper_price,
            long_exposure_units: self.long_exposure_units,
            short_exposure_units: self.short_exposure_units,
            notional_per_unit: self.notional_per_unit,
            shape_family: self.shape_family,
            out_of_band_policy: self.out_of_band_policy,
        }
    }

    pub fn budget(&self) -> CapacityBudget {
        CapacityBudget {
            max_notional: self.long_exposure_units.max(self.short_exposure_units)
                * self.notional_per_unit,
            daily_loss_limit: f64::NEG_INFINITY,
            stop_loss_pct: 100.0,
        }
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

#[cfg(test)]
mod tests {
    use grid_core::strategy::{OutOfBandPolicy, ShapeFamily};

    use super::{Config, default_bind_address, parse_config};

    #[test]
    fn parses_config_file_with_grids_and_exchange() {
        let config = parse_config(
            r#"
environment = "test"
bind_address = "127.0.0.1:9000"

[exchange]
api_key = "demo-key"
api_secret = "demo-secret"
rest_base_url = "http://127.0.0.1:18080"
ws_base_url = "ws://127.0.0.1:18081"

[[grids]]
grid_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 6.0
notional_per_unit = 3000.0

[[grids]]
grid_id = "eth-core"
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
        assert_eq!(config.grids.len(), 2);
        assert_eq!(config.grids[0].symbol, "BTCUSDT");
        assert_eq!(config.grids[0].grid_id().as_str(), "btc-core");
        assert_eq!(
            config.grids[1].shape_family,
            grid_core::strategy::ShapeFamily::Concave
        );
        assert_eq!(
            config.grids[1].out_of_band_policy,
            grid_core::strategy::OutOfBandPolicy::Hold
        );
        assert_eq!(config.exchange.api_key.as_deref(), Some("demo-key"));
        assert_eq!(
            config.exchange.ws_base_url.as_deref(),
            Some("ws://127.0.0.1:18081")
        );
    }

    #[test]
    fn defaults_bind_address_and_exchange_credentials() {
        let config = parse_config(
            r#"
environment = "paper"

[[grids]]
grid_id = "btc-core"
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
            config.grids[0].grid_config().shape_family,
            grid_core::strategy::ShapeFamily::Linear
        );
        assert_eq!(
            config.grids[0].grid_config().out_of_band_policy,
            grid_core::strategy::OutOfBandPolicy::Freeze
        );
    }

    #[test]
    fn parses_snake_case_strategy_enums() {
        let config = parse_config(
            r#"
environment = "paper"

[[grids]]
grid_id = "btc-core"
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

        let grid = &config.grids[0];
        assert_eq!(grid.shape_family, ShapeFamily::Concave);
        assert_eq!(grid.out_of_band_policy, OutOfBandPolicy::ReduceOnly);
        assert_eq!(grid.budget().max_notional, 3000.0);
    }

    #[test]
    fn computes_default_db_path_from_environment() {
        let config = Config {
            environment: "testnet".into(),
            bind_address: default_bind_address(),
            grids: vec![],
            exchange: Default::default(),
        };

        assert_eq!(
            config.default_db_path(),
            std::path::Path::new(".data")
                .join("testnet")
                .join("grid-server.sqlite")
        );
    }

    #[test]
    fn parses_explicit_grid_id_from_config_instead_of_deriving_from_symbol() {
        let config = parse_config(
            r#"
environment = "paper"

[[grids]]
grid_id = "btc-core"
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

        assert_eq!(config.grids[0].symbol, "BTCUSDT");
        assert_eq!(config.grids[0].grid_id().as_str(), "btc-core");
    }
}
