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
    pub fn credentials(&self) -> Result<Credentials> {
        Ok(Credentials {
            private_key: normalize_hex(
                required_field(self.private_key.as_deref(), "exchange.private_key")?,
                "exchange.private_key",
                64,
            )?,
            wallet_address: normalize_hex(
                required_field(self.wallet_address.as_deref(), "exchange.wallet_address")?,
                "exchange.wallet_address",
                40,
            )?,
            vault_address: normalize_optional_hex(
                self.vault_address.as_deref(),
                "exchange.vault_address",
                40,
            )?,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    private_key: String,
    wallet_address: String,
    vault_address: Option<String>,
}

impl Credentials {
    pub fn private_key(&self) -> &str {
        &self.private_key
    }

    pub fn wallet_address(&self) -> &str {
        &self.wallet_address
    }

    pub fn vault_address(&self) -> Option<&str> {
        self.vault_address.as_deref()
    }
}

fn required_field(value: Option<&str>, field_name: &str) -> Result<String> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing required {field_name}"))?;
    Ok(value.to_string())
}

fn normalize_optional_hex(
    value: Option<&str>,
    field_name: &str,
    expected_hex_chars: usize,
) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    normalize_hex(value, field_name, expected_hex_chars).map(Some)
}

fn normalize_hex(
    value: impl AsRef<str>,
    field_name: &str,
    expected_hex_chars: usize,
) -> Result<String> {
    let normalized = value.as_ref().trim().to_ascii_lowercase();
    let hex = normalized
        .strip_prefix("0x")
        .ok_or_else(|| anyhow!("invalid {field_name}: expected 0x-prefixed hex"))?;
    if hex.len() != expected_hex_chars || !hex.chars().all(|value| value.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "invalid {field_name}: expected 0x plus {expected_hex_chars} hex characters"
        ));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_resolves_mainnet_and_testnet_endpoints() {
        assert_eq!(
            Deployment::Mainnet.endpoints(),
            Endpoints::new(
                "https://api.hyperliquid.xyz",
                "wss://api.hyperliquid.xyz/ws"
            )
        );
        assert_eq!(
            Deployment::Testnet.endpoints(),
            Endpoints::new(
                "https://api.hyperliquid-testnet.xyz",
                "wss://api.hyperliquid-testnet.xyz/ws"
            )
        );
    }

    #[test]
    fn credentials_validate_required_fields_hex_private_key_and_addresses() {
        enum Expectation {
            Err(&'static str),
            Ok {
                private_key: &'static str,
                wallet_address: &'static str,
                vault_address: Option<&'static str>,
            },
        }

        struct Case {
            name: &'static str,
            private_key: Option<&'static str>,
            wallet_address: Option<&'static str>,
            vault_address: Option<&'static str>,
            expect: Expectation,
        }

        let valid_key = "0x1111111111111111111111111111111111111111111111111111111111111111";
        let valid_wallet = "0x2222222222222222222222222222222222222222";
        let valid_vault = "0x3333333333333333333333333333333333333333";
        let cases = [
            Case {
                name: "missing private key",
                private_key: None,
                wallet_address: Some(valid_wallet),
                vault_address: None,
                expect: Expectation::Err("missing required exchange.private_key"),
            },
            Case {
                name: "missing wallet address",
                private_key: Some(valid_key),
                wallet_address: None,
                vault_address: None,
                expect: Expectation::Err("missing required exchange.wallet_address"),
            },
            Case {
                name: "invalid private key",
                private_key: Some("0x1234"),
                wallet_address: Some(valid_wallet),
                vault_address: None,
                expect: Expectation::Err("invalid exchange.private_key"),
            },
            Case {
                name: "invalid wallet address",
                private_key: Some(valid_key),
                wallet_address: Some("0x1234"),
                vault_address: None,
                expect: Expectation::Err("invalid exchange.wallet_address"),
            },
            Case {
                name: "invalid vault address",
                private_key: Some(valid_key),
                wallet_address: Some(valid_wallet),
                vault_address: Some("vault"),
                expect: Expectation::Err("invalid exchange.vault_address"),
            },
            Case {
                name: "trims key and lowercases addresses",
                private_key: Some(
                    "  0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA  ",
                ),
                wallet_address: Some("  0xBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB  "),
                vault_address: Some("  0xCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC  "),
                expect: Expectation::Ok {
                    private_key: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    wallet_address: "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    vault_address: Some("0xcccccccccccccccccccccccccccccccccccccccc"),
                },
            },
            Case {
                name: "success without vault address",
                private_key: Some(valid_key),
                wallet_address: Some(valid_wallet),
                vault_address: None,
                expect: Expectation::Ok {
                    private_key: valid_key,
                    wallet_address: valid_wallet,
                    vault_address: None,
                },
            },
            Case {
                name: "success with vault address",
                private_key: Some(valid_key),
                wallet_address: Some(valid_wallet),
                vault_address: Some(valid_vault),
                expect: Expectation::Ok {
                    private_key: valid_key,
                    wallet_address: valid_wallet,
                    vault_address: Some(valid_vault),
                },
            },
        ];

        for case in cases {
            let config = Config {
                deployment: Deployment::Testnet,
                private_key: case.private_key.map(str::to_string),
                wallet_address: case.wallet_address.map(str::to_string),
                vault_address: case.vault_address.map(str::to_string),
            };

            match case.expect {
                Expectation::Err(expected) => {
                    let error = config.credentials().unwrap_err();
                    assert!(
                        error.to_string().contains(expected),
                        "case `{}` produced `{}`",
                        case.name,
                        error
                    );
                }
                Expectation::Ok {
                    private_key,
                    wallet_address,
                    vault_address,
                } => {
                    let credentials = config.credentials().unwrap();
                    assert_eq!(credentials.private_key(), private_key, "case `{}`", case.name);
                    assert_eq!(
                        credentials.wallet_address(),
                        wallet_address,
                        "case `{}`",
                        case.name
                    );
                    assert_eq!(
                        credentials.vault_address(),
                        vault_address,
                        "case `{}`",
                        case.name
                    );
                }
            }
        }
    }
}
