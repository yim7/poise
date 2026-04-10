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
}

impl Deployment {
    pub fn endpoints(&self) -> Endpoints {
        match self {
            Self::Mainnet => Endpoints::new(
                "https://api.bybit.com",
                "wss://stream.bybit.com/v5/public/linear",
                "wss://stream.bybit.com/v5/private",
            ),
            Self::Testnet => Endpoints::new(
                "https://api-testnet.bybit.com",
                "wss://stream-testnet.bybit.com/v5/public/linear",
                "wss://stream-testnet.bybit.com/v5/private",
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    rest_base_url: String,
    public_ws_base_url: String,
    private_ws_base_url: String,
}

impl Endpoints {
    pub fn new(
        rest_base_url: impl Into<String>,
        public_ws_base_url: impl Into<String>,
        private_ws_base_url: impl Into<String>,
    ) -> Self {
        Self {
            rest_base_url: rest_base_url.into().trim_end_matches('/').to_string(),
            public_ws_base_url: public_ws_base_url.into().trim_end_matches('/').to_string(),
            private_ws_base_url: private_ws_base_url.into().trim_end_matches('/').to_string(),
        }
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
    fn deployment_resolves_mainnet_and_testnet_endpoints() {
        assert_eq!(
            Deployment::Mainnet.endpoints(),
            Endpoints::new(
                "https://api.bybit.com",
                "wss://stream.bybit.com/v5/public/linear",
                "wss://stream.bybit.com/v5/private",
            )
        );
        assert_eq!(
            Deployment::Testnet.endpoints(),
            Endpoints::new(
                "https://api-testnet.bybit.com",
                "wss://stream-testnet.bybit.com/v5/public/linear",
                "wss://stream-testnet.bybit.com/v5/private",
            )
        );
    }

    #[test]
    fn credentials_require_api_key_and_api_secret() {
        let error = Config::default().credentials().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("missing required exchange.api_key")
        );
    }
}
