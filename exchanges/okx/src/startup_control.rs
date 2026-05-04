use anyhow::{Result, anyhow};

use crate::Config;

pub struct SymbolLeverageControl;

impl SymbolLeverageControl {
    pub fn new(config: &Config) -> Result<Self> {
        let _credentials = config.credentials()?;
        Ok(Self)
    }

    pub async fn set_leverage(&self, _symbol: &str, _leverage: u32) -> Result<()> {
        Err(anyhow!(
            "OKX leverage control is pending REST client wiring"
        ))
    }
}
