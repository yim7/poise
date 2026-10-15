//! [`Account`] 的 Hyperliquid 实现：账户模式、API 钱包、可用资金、费率、合约规则。

use async_trait::async_trait;
use chrono::DateTime;
use poise_runtime::{Account, AccountMode, ApiWallet, ExchangeError};
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::units::{Notional, Rate};
use serde_json::{Value, json};

use crate::client::Hyperliquid;
use crate::models::{AgentResponse, SpotClearinghouseState, UserFees};

/// USDC 在统一账户里的 token 序号。
const USDC_TOKEN: u32 = 0;

#[async_trait]
impl Account for Hyperliquid {
    async fn mode(&self) -> Result<AccountMode, ExchangeError> {
        let body = json!({
            "type": "userAbstraction",
            "user": self.user(),
        });
        let value: Value = self.info(&body, "userAbstraction").await?;
        // 读不出模式时不能当成 Unified：模式判断错了，后面的资金和保证金判断全是错的。
        Ok(match value.as_str() {
            Some("unifiedAccount") => AccountMode::Unified,
            Some(other) => AccountMode::Other(other.to_string()),
            None => AccountMode::Other("<unknown>".to_string()),
        })
    }

    async fn authorized_api_wallets(&self) -> Result<Vec<ApiWallet>, ExchangeError> {
        let body = json!({
            "type": "extraAgents",
            "user": self.user(),
        });
        let agents: Vec<Option<AgentResponse>> = self.info(&body, "extraAgents").await?;
        agents
            .into_iter()
            .flatten()
            .map(|agent| {
                // 解析不了就等于证明不了「这个私钥是授权的」，宁可拒绝。
                let address = poise_runtime::Address::parse(&agent.address).map_err(|_| {
                    ExchangeError::Unavailable {
                        reason: "the exchange returned an API wallet with an unreadable address"
                            .to_string(),
                    }
                })?;
                let valid_until =
                    DateTime::from_timestamp_millis(agent.valid_until).ok_or_else(|| {
                        ExchangeError::Unavailable {
                            reason: "the exchange returned an API wallet with an unreadable expiry"
                                .to_string(),
                        }
                    })?;
                Ok(ApiWallet {
                    name: agent.name,
                    address,
                    valid_until,
                })
            })
            .collect()
    }

    async fn available_collateral(&self) -> Result<Notional, ExchangeError> {
        let body = json!({
            "type": "spotClearinghouseState",
            "user": self.user(),
        });
        let state: SpotClearinghouseState = self.info(&body, "spotClearinghouseState").await?;
        // 维持保证金之后仍可用的量，不是可提现量。
        if let Some((_, available)) = state
            .token_to_available_after_maintenance
            .iter()
            .find(|(token, _)| *token == USDC_TOKEN)
        {
            return Notional::parse(available).map_err(|_| ExchangeError::Unavailable {
                reason: "the exchange returned an unreadable available balance".to_string(),
            });
        }
        // 有些账户模式下交易所不给这一项，退回到「总额 − 冻结」。
        let balance = state
            .balances
            .iter()
            // `coin` 是名字，`token` 是交叉核对：交易所没给 `token` 时按名字认，
            // 不能因为少一个可选字段就说「没有 USDC 余额」。
            .find(|balance| {
                balance.coin == "USDC" && balance.token.is_none_or(|token| token == USDC_TOKEN)
            })
            .ok_or_else(|| ExchangeError::Unavailable {
                reason: "the exchange did not return a USDC balance".to_string(),
            })?;
        let total = Notional::parse(&balance.total).map_err(|_| unreadable("balance"))?;
        let hold = Notional::parse(&balance.hold).map_err(|_| unreadable("hold"))?;
        Ok(Notional::from_scaled(
            total.scaled_floor() - hold.scaled_floor(),
        ))
    }

    async fn fees(&self) -> Result<Fees, ExchangeError> {
        let body = json!({
            "type": "userFees",
            "user": self.user(),
        });
        let fees: UserFees = self.info(&body, "userFees").await?;
        Ok(Fees::new(
            Rate::parse(&fees.user_cross_rate).map_err(|_| unreadable("taker rate"))?,
            Rate::parse(&fees.user_add_rate).map_err(|_| unreadable("maker rate"))?,
        ))
    }

    async fn instrument_spec(
        &self,
        instrument: &Instrument,
    ) -> Result<InstrumentSpec, ExchangeError> {
        // 报价规则本身随价格变化，不必先取标记价。
        let asset = self.asset(instrument).await?;
        asset
            .instrument_spec(instrument.clone())
            .map_err(|reason| ExchangeError::Unavailable { reason })
    }
}

fn unreadable(what: &str) -> ExchangeError {
    ExchangeError::Unavailable {
        reason: format!("the exchange returned an unreadable {what}"),
    }
}
