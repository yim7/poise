use anyhow::{Context, Result};
use grid_core::risk::CapacityBudget;
use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Config {
    pub environment: String,
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    pub instances: Vec<InstanceConfig>,
    #[serde(default)]
    pub exchange: ExchangeConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InstanceConfig {
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_capacity: f64,
    pub short_capacity: f64,
    pub capacity_notional: f64,
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

impl InstanceConfig {
    pub fn instance_id(&self) -> String {
        self.symbol.clone()
    }

    pub fn grid_config(&self) -> GridConfig {
        GridConfig {
            lower_price: self.lower_price,
            upper_price: self.upper_price,
            long_capacity: self.long_capacity,
            short_capacity: self.short_capacity,
            capacity_notional: self.capacity_notional,
            shape_family: self.shape_family,
            out_of_band_policy: self.out_of_band_policy,
        }
    }

    pub fn budget(&self) -> CapacityBudget {
        CapacityBudget {
            max_notional: self.capacity_notional,
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
    use super::{Config, default_bind_address, parse_config};

    #[test]
    fn parses_config_file_with_instances_and_exchange() {
        let config = parse_config(
            r#"
environment = "test"
bind_address = "127.0.0.1:9000"

[exchange]
api_key = "demo-key"
api_secret = "demo-secret"
rest_base_url = "http://127.0.0.1:18080"
ws_base_url = "ws://127.0.0.1:18081"

[[instances]]
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_capacity = 8.0
short_capacity = 6.0
capacity_notional = 3000.0

[[instances]]
symbol = "ETHUSDT"
lower_price = 2000.0
upper_price = 2600.0
long_capacity = 5.0
short_capacity = 4.0
capacity_notional = 2000.0
shape_family = "Concave"
out_of_band_policy = "Hold"
"#,
        )
        .unwrap();

        assert_eq!(config.environment, "test");
        assert_eq!(config.bind_address, "127.0.0.1:9000");
        assert_eq!(config.instances.len(), 2);
        assert_eq!(config.instances[0].symbol, "BTCUSDT");
        assert_eq!(
            config.instances[1].shape_family,
            grid_core::strategy::ShapeFamily::Concave
        );
        assert_eq!(
            config.instances[1].out_of_band_policy,
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

[[instances]]
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_capacity = 8.0
short_capacity = 8.0
capacity_notional = 375.0
"#,
        )
        .unwrap();

        assert_eq!(config.bind_address, default_bind_address());
        assert_eq!(config.exchange.api_key, None);
        assert_eq!(config.exchange.api_secret, None);
        assert_eq!(
            config.instances[0].grid_config().shape_family,
            grid_core::strategy::ShapeFamily::Linear
        );
        assert_eq!(
            config.instances[0].grid_config().out_of_band_policy,
            grid_core::strategy::OutOfBandPolicy::Freeze
        );
    }

    #[test]
    fn computes_default_db_path_from_environment() {
        let config = Config {
            environment: "testnet".into(),
            bind_address: default_bind_address(),
            instances: vec![],
            exchange: Default::default(),
        };

        assert_eq!(
            config.default_db_path(),
            std::path::Path::new(".data")
                .join("testnet")
                .join("grid-server.sqlite")
        );
    }
}
