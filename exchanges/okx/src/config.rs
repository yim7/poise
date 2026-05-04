use anyhow::{Result, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub deployment: Deployment,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub passphrase: Option<String>,
}

impl Config {
    pub fn credentials(&self) -> Result<Credentials> {
        Ok(Credentials {
            api_key: required_field(self.api_key.as_deref(), "exchange.api_key")?,
            api_secret: required_field(self.api_secret.as_deref(), "exchange.api_secret")?,
            passphrase: required_field(self.passphrase.as_deref(), "exchange.passphrase")?,
        })
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
    Demo,
}

impl Deployment {
    pub fn endpoints(&self) -> Endpoints {
        match self {
            Self::Mainnet => Endpoints::new(
                "https://www.okx.com",
                "wss://ws.okx.com:8443/ws/v5/public",
                "wss://ws.okx.com:8443/ws/v5/private",
                "wss://ws.okx.com:8443/ws/v5/business",
                false,
            ),
            Self::Demo => Endpoints::new(
                "https://www.okx.com",
                "wss://wspap.okx.com:8443/ws/v5/public",
                "wss://wspap.okx.com:8443/ws/v5/private",
                "wss://wspap.okx.com:8443/ws/v5/business",
                true,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    rest_base_url: String,
    public_ws_url: String,
    private_ws_url: String,
    business_ws_url: String,
    simulated_trading: bool,
}

impl Endpoints {
    pub fn new(
        rest_base_url: impl Into<String>,
        public_ws_url: impl Into<String>,
        private_ws_url: impl Into<String>,
        business_ws_url: impl Into<String>,
        simulated_trading: bool,
    ) -> Self {
        Self {
            rest_base_url: rest_base_url.into().trim_end_matches('/').to_string(),
            public_ws_url: public_ws_url.into().trim_end_matches('/').to_string(),
            private_ws_url: private_ws_url.into().trim_end_matches('/').to_string(),
            business_ws_url: business_ws_url.into().trim_end_matches('/').to_string(),
            simulated_trading,
        }
    }

    pub fn rest_base_url(&self) -> &str {
        &self.rest_base_url
    }

    pub fn public_ws_url(&self) -> &str {
        &self.public_ws_url
    }

    pub fn private_ws_url(&self) -> &str {
        &self.private_ws_url
    }

    pub fn business_ws_url(&self) -> &str {
        &self.business_ws_url
    }

    pub fn simulated_trading(&self) -> bool {
        self.simulated_trading
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    api_key: String,
    api_secret: String,
    passphrase: String,
}

impl Credentials {
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn api_secret(&self) -> &str {
        &self.api_secret
    }

    pub fn passphrase(&self) -> &str {
        &self.passphrase
    }
}

fn required_field(value: Option<&str>, field_name: &str) -> Result<String> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing required {field_name}"))?;
    Ok(value.to_string())
}
