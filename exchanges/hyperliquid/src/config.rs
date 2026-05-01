use anyhow::{Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub deployment: Deployment,
    pub private_key: Option<String>,
    pub wallet_address: Option<String>,
    pub vault_address: Option<String>,
}

impl Config {
    pub fn credentials(&self) -> Result<(String, String)> {
        Ok((
            required_field(self.private_key.as_deref(), "exchange.private_key")?,
            required_field(self.wallet_address.as_deref(), "exchange.wallet_address")?,
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
}

impl Deployment {
    pub fn endpoints(&self) -> Endpoints {
        match self {
            Self::Mainnet => Endpoints::new(
                "https://api.hyperliquid.xyz",
                "wss://api.hyperliquid.xyz/ws",
            ),
            Self::Testnet => Endpoints::new(
                "https://api.hyperliquid-testnet.xyz",
                "wss://api.hyperliquid-testnet.xyz/ws",
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    rest_base_url: String,
    ws_url: String,
}

impl Endpoints {
    pub fn new(rest_base_url: impl Into<String>, ws_url: impl Into<String>) -> Self {
        Self {
            rest_base_url: rest_base_url.into().trim_end_matches('/').to_string(),
            ws_url: ws_url.into().to_string(),
        }
    }

    pub fn rest_base_url(&self) -> &str {
        &self.rest_base_url
    }

    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }
}

fn required_field(value: Option<&str>, field_name: &str) -> Result<String> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing required {field_name}"))?;
    Ok(value.to_string())
}
