use anyhow::{Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub deployment: Deployment,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
}

impl Config {
    pub fn credentials(&self) -> Result<(String, String)> {
        Ok((
            required_field(self.api_key.as_deref(), "exchange.api_key")?,
            required_field(self.api_secret.as_deref(), "exchange.api_secret")?,
        ))
    }

    pub fn endpoints(&self) -> Endpoints {
        self.deployment.endpoints()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deployment {
    Mainnet,
    #[default]
    Testnet,
    Custom {
        rest_base_url: String,
        ws_base_url: String,
    },
}

impl Deployment {
    pub fn endpoints(&self) -> Endpoints {
        match self {
            Self::Mainnet => {
                Endpoints::new("https://fapi.binance.com", "wss://fstream.binance.com")
            }
            Self::Testnet => Endpoints::new(
                "https://demo-fapi.binance.com",
                "wss://fstream.binancefuture.com",
            ),
            Self::Custom {
                rest_base_url,
                ws_base_url,
            } => Endpoints::new(rest_base_url.clone(), ws_base_url.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    rest_base_url: String,
    ws_base_url: String,
}

impl Endpoints {
    pub fn new(rest_base_url: impl Into<String>, ws_base_url: impl Into<String>) -> Self {
        Self {
            rest_base_url: rest_base_url.into().trim_end_matches('/').to_string(),
            ws_base_url: ws_base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn rest_base_url(&self) -> &str {
        &self.rest_base_url
    }

    pub fn ws_base_url(&self) -> &str {
        &self.ws_base_url
    }
}

fn required_field(value: Option<&str>, field_name: &str) -> Result<String> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing required {field_name}"))?;
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_resolves_mainnet_testnet_and_custom_endpoints() {
        assert_eq!(
            Deployment::Mainnet.endpoints(),
            Endpoints::new("https://fapi.binance.com", "wss://fstream.binance.com")
        );
        assert_eq!(
            Deployment::Testnet.endpoints(),
            Endpoints::new("https://demo-fapi.binance.com", "wss://fstream.binancefuture.com")
        );
        assert_eq!(
            Deployment::Custom {
                rest_base_url: "http://127.0.0.1:8080".to_string(),
                ws_base_url: "ws://127.0.0.1:9000".to_string(),
            }
            .endpoints(),
            Endpoints::new("http://127.0.0.1:8080", "ws://127.0.0.1:9000")
        );
    }
}
