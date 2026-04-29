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
        #[serde(alias = "ws_base_url")]
        ws_root_base_url: String,
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
                ws_root_base_url,
            } => Endpoints::new(rest_base_url.clone(), ws_root_base_url.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    rest_base_url: String,
    public_ws_base_url: String,
    market_ws_base_url: String,
    user_ws_base_url: String,
}

impl Endpoints {
    pub fn new(rest_base_url: impl Into<String>, ws_root_base_url: impl Into<String>) -> Self {
        let ws_root_base_url = ws_root_base_url.into().trim_end_matches('/').to_string();
        Self {
            rest_base_url: rest_base_url.into().trim_end_matches('/').to_string(),
            public_ws_base_url: routed_ws_base_url(&ws_root_base_url, "public"),
            market_ws_base_url: routed_ws_base_url(&ws_root_base_url, "market"),
            user_ws_base_url: routed_ws_base_url(&ws_root_base_url, "private"),
        }
    }

    pub fn rest_base_url(&self) -> &str {
        &self.rest_base_url
    }

    pub fn public_ws_base_url(&self) -> &str {
        &self.public_ws_base_url
    }

    pub fn market_ws_base_url(&self) -> &str {
        &self.market_ws_base_url
    }

    pub fn user_ws_base_url(&self) -> &str {
        &self.user_ws_base_url
    }
}

fn routed_ws_base_url(root_base_url: &str, route: &str) -> String {
    format!("{root_base_url}/{route}")
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
        let mainnet = Deployment::Mainnet.endpoints();
        assert_eq!(mainnet.rest_base_url(), "https://fapi.binance.com");
        assert_eq!(
            mainnet.public_ws_base_url(),
            "wss://fstream.binance.com/public"
        );
        assert_eq!(
            mainnet.market_ws_base_url(),
            "wss://fstream.binance.com/market"
        );
        assert_eq!(
            mainnet.user_ws_base_url(),
            "wss://fstream.binance.com/private"
        );

        let testnet = Deployment::Testnet.endpoints();
        assert_eq!(testnet.rest_base_url(), "https://demo-fapi.binance.com");
        assert_eq!(
            testnet.public_ws_base_url(),
            "wss://fstream.binancefuture.com/public"
        );
        assert_eq!(
            testnet.market_ws_base_url(),
            "wss://fstream.binancefuture.com/market"
        );
        assert_eq!(
            testnet.user_ws_base_url(),
            "wss://fstream.binancefuture.com/private"
        );

        let custom = Deployment::Custom {
            rest_base_url: "http://127.0.0.1:8080".to_string(),
            ws_root_base_url: "ws://127.0.0.1:9000".to_string(),
        }
        .endpoints();
        assert_eq!(custom.rest_base_url(), "http://127.0.0.1:8080");
        assert_eq!(custom.public_ws_base_url(), "ws://127.0.0.1:9000/public");
        assert_eq!(custom.market_ws_base_url(), "ws://127.0.0.1:9000/market");
        assert_eq!(custom.user_ws_base_url(), "ws://127.0.0.1:9000/private");
    }

    #[test]
    fn custom_deployment_accepts_legacy_ws_base_url_field() {
        let deployment: Deployment = serde_json::from_str(
            r#"{"custom":{"rest_base_url":"http://127.0.0.1:8080","ws_base_url":"ws://127.0.0.1:9000"}}"#,
        )
        .unwrap();

        let Deployment::Custom {
            ws_root_base_url, ..
        } = deployment
        else {
            panic!("expected custom deployment");
        };
        assert_eq!(ws_root_base_url, "ws://127.0.0.1:9000");
    }
}
