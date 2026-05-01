use std::sync::Arc;

use anyhow::Result;

use crate::{Config, rest::client::HyperliquidRestClient};

pub struct SymbolLeverageControl {
    rest: Arc<HyperliquidRestClient>,
}

impl SymbolLeverageControl {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            rest: Arc::new(HyperliquidRestClient::new(config)?),
        })
    }

    pub async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        self.rest.set_leverage(symbol, leverage).await
    }
}
