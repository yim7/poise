use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServiceConfig {
    pub environment: String,
    pub default_symbol: Option<String>,
    #[serde(default)]
    pub instances: Vec<InstanceConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InstanceConfig {
    pub symbol: String,
    pub range: GridRangeConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GridRangeConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub grid_levels: u32,
    pub max_position_notional: f64,
}

impl ServiceConfig {
    pub fn from_toml_str(input: &str) -> Result<Self> {
        let config = toml::from_str::<Self>(input).context("failed to parse config TOML")?;
        config.validate_and_normalize()
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        Self::from_toml_str(&content)
            .with_context(|| format!("failed to validate config file {}", path.display()))
    }

    fn validate_and_normalize(mut self) -> Result<Self> {
        self.environment = self.environment.trim().to_string();
        ensure!(
            !self.environment.is_empty(),
            "environment must not be empty"
        );
        ensure!(
            !self.instances.is_empty(),
            "instances must contain at least one entry"
        );

        let mut symbols = HashSet::new();
        for instance in &mut self.instances {
            instance.symbol = normalize_symbol(&instance.symbol);
            ensure!(
                !instance.symbol.is_empty(),
                "instance symbol must not be empty"
            );
            ensure!(
                symbols.insert(instance.symbol.clone()),
                "duplicate instance symbol `{}`",
                instance.symbol
            );
            instance.range.validate(&instance.symbol)?;
        }

        if let Some(default_symbol) = &mut self.default_symbol {
            *default_symbol = normalize_symbol(default_symbol);
            ensure!(
                !default_symbol.is_empty(),
                "default_symbol must not be empty when provided"
            );
            ensure!(
                symbols.contains(default_symbol),
                "default_symbol `{default_symbol}` must exist in instances"
            );
        }

        Ok(self)
    }
}

pub fn default_sqlite_path(environment: &str, symbol: &str) -> PathBuf {
    PathBuf::from(".data")
        .join(environment.trim())
        .join(format!("{}.db", symbol.trim().to_ascii_lowercase()))
}

fn normalize_symbol(symbol: &str) -> String {
    symbol.trim().to_ascii_uppercase()
}

impl GridRangeConfig {
    fn validate(&self, symbol: &str) -> Result<()> {
        ensure!(
            self.lower_price < self.upper_price,
            "instance `{symbol}` lower_price must be less than upper_price"
        );
        ensure!(
            self.grid_levels >= 2,
            "instance `{symbol}` grid_levels must be at least 2"
        );
        ensure!(
            self.max_position_notional > 0.0,
            "instance `{symbol}` max_position_notional must be greater than 0"
        );
        if !self.lower_price.is_finite() || !self.upper_price.is_finite() {
            return Err(anyhow!(
                "instance `{symbol}` lower_price and upper_price must be finite"
            ));
        }
        if !self.max_position_notional.is_finite() {
            return Err(anyhow!(
                "instance `{symbol}` max_position_notional must be finite"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ServiceConfig, default_sqlite_path};

    #[test]
    fn parses_and_validates_single_environment_config() {
        let config = ServiceConfig::from_toml_str(
            r#"
environment = "testnet"
default_symbol = "BTCUSDT"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 90000.0
upper_price = 110000.0
grid_levels = 8
max_position_notional = 25000.0

[[instances]]
symbol = "ETHUSDT"

[instances.range]
lower_price = 2000.0
upper_price = 4000.0
grid_levels = 6
max_position_notional = 12000.0
"#,
        )
        .expect("valid config should parse");

        assert_eq!(config.environment, "testnet");
        assert_eq!(config.default_symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(config.instances.len(), 2);
        assert_eq!(config.instances[1].symbol, "ETHUSDT");
        assert_eq!(config.instances[0].range.grid_levels, 8);
    }

    #[test]
    fn normalizes_symbols_before_duplicate_and_default_symbol_validation() {
        let normalized = ServiceConfig::from_toml_str(
            r#"
environment = "prod"
default_symbol = "btcusdt"

[[instances]]
symbol = " btcusdt "

[instances.range]
lower_price = 1.0
upper_price = 2.0
grid_levels = 2
max_position_notional = 1.0
"#,
        )
        .expect("config with mixed-case symbol should parse");
        assert_eq!(normalized.default_symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(normalized.instances[0].symbol, "BTCUSDT");

        let error = ServiceConfig::from_toml_str(
            r#"
environment = "prod"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 1.0
upper_price = 2.0
grid_levels = 2
max_position_notional = 1.0

[[instances]]
symbol = "btcusdt"

[instances.range]
lower_price = 2.0
upper_price = 3.0
grid_levels = 2
max_position_notional = 1.0
"#,
        )
        .expect_err("case-insensitive duplicate symbols must be rejected");
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn rejects_invalid_config_values() {
        let cases = [
            (
                "empty environment",
                r#"
environment = "   "

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 1.0
upper_price = 2.0
grid_levels = 2
max_position_notional = 1.0
"#,
                "environment",
            ),
            (
                "blank symbol",
                r#"
environment = "prod"

[[instances]]
symbol = "   "

[instances.range]
lower_price = 1.0
upper_price = 2.0
grid_levels = 2
max_position_notional = 1.0
"#,
                "symbol",
            ),
            (
                "duplicate symbol after trim",
                r#"
environment = "prod"

[[instances]]
symbol = " BTCUSDT "

[instances.range]
lower_price = 1.0
upper_price = 2.0
grid_levels = 2
max_position_notional = 1.0

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 2.0
upper_price = 3.0
grid_levels = 2
max_position_notional = 1.0
"#,
                "duplicate",
            ),
            (
                "empty instances",
                r#"
environment = "prod"
"#,
                "instances",
            ),
            (
                "invalid range bounds",
                r#"
environment = "prod"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 3.0
upper_price = 3.0
grid_levels = 2
max_position_notional = 1.0
"#,
                "lower_price",
            ),
            (
                "grid levels too small",
                r#"
environment = "prod"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 1.0
upper_price = 3.0
grid_levels = 1
max_position_notional = 1.0
"#,
                "grid_levels",
            ),
            (
                "max notional not positive",
                r#"
environment = "prod"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 1.0
upper_price = 3.0
grid_levels = 2
max_position_notional = 0.0
"#,
                "max_position_notional",
            ),
            (
                "default symbol missing",
                r#"
environment = "prod"
default_symbol = "ETHUSDT"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 1.0
upper_price = 3.0
grid_levels = 2
max_position_notional = 1.0
"#,
                "default_symbol",
            ),
        ];

        for (name, input, expected_message) in cases {
            let error = ServiceConfig::from_toml_str(input).expect_err(name);
            assert!(
                error.to_string().contains(expected_message),
                "case `{name}` error was `{error}`"
            );
        }
    }

    #[test]
    fn builds_default_sqlite_path_from_environment_and_symbol() {
        let path = default_sqlite_path("prod", "BtcUsdt");
        assert_eq!(path, std::path::PathBuf::from(".data/prod/btcusdt.db"));
    }
}
