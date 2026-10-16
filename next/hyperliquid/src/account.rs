//! [`Account`] 的 Hyperliquid 实现：账户模式、API 钱包、可用资金、费率、合约规则。

use async_trait::async_trait;
use chrono::DateTime;
use poise_runtime::account::{Leverage, MarginReading};
use poise_runtime::{Account, AccountMode, ApiWallet, ExchangeError};
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::stress::{MarginTier, MarginTiers};
use poise_strategy::units::{Notional, Rate};
use serde_json::{Value, json};

use crate::client::Hyperliquid;
use crate::models::{
    ActiveAssetData, AgentResponse, ClearinghouseState, MetaResponse, SpotClearinghouseState,
    UserFees,
};

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

    /// 账户比率的组成部分（第 7.4 节，附录 A）：现货账户状态里的 USDC 余额和维持保证金之后
    /// 的可用量（覆盖整个账户），以及默认 DEX 合约账户状态里的全仓维持保证金占用、逐仓仓位
    /// 占用的保证金。
    ///
    /// 任何一项读不出来都报「读不到」，闸门随之关上：拿一份缺了一块的读数当正常比率，只会
    /// 低估风险。
    async fn margin(&self) -> Result<MarginReading, ExchangeError> {
        let spot: SpotClearinghouseState = self
            .info(
                &json!({"type": "spotClearinghouseState", "user": self.user()}),
                "spotClearinghouseState",
            )
            .await?;
        let perp: ClearinghouseState = self
            .info(
                &json!({"type": "clearinghouseState", "user": self.user()}),
                "clearinghouseState",
            )
            .await?;
        let balance = spot
            .balances
            .iter()
            .find(|balance| {
                balance.coin == "USDC" && balance.token.is_none_or(|token| token == USDC_TOKEN)
            })
            .ok_or_else(|| ExchangeError::Unavailable {
                reason: "the exchange did not return a USDC balance".to_string(),
            })?;
        let balance = Notional::parse(&balance.total).map_err(|_| unreadable("balance"))?;
        let available_after_maintenance = spot
            .token_to_available_after_maintenance
            .iter()
            .find(|(token, _)| *token == USDC_TOKEN)
            .ok_or_else(|| unreadable("available balance after maintenance"))
            .and_then(|(_, available)| {
                Notional::parse(available)
                    .map_err(|_| unreadable("available balance after maintenance"))
            })?;
        let cross_maintenance = notional(
            perp.cross_maintenance_margin_used.as_deref(),
            "cross maintenance margin",
        )?;
        let mut isolated_margin = Notional::ZERO;
        for position in &perp.asset_positions {
            let position = &position.position;
            let kind = position
                .leverage
                .as_ref()
                .map(|leverage| leverage.kind.as_str());
            match kind {
                Some("cross") => {}
                Some("isolated") => {
                    let used = notional(position.margin_used.as_deref(), "isolated margin")?;
                    isolated_margin =
                        Notional::from_scaled(isolated_margin.scaled_floor() + used.scaled_floor());
                }
                // 不知道是全仓还是逐仓，就不知道要不要从分母里扣掉它的保证金。
                _ => return Err(unreadable("position margin mode")),
            }
        }
        Ok(MarginReading {
            balance,
            available_after_maintenance,
            cross_maintenance,
            isolated_margin,
            // 没有时刻就判断不了读数有没有过期：当成读不到，不能当成刚读的。
            as_of: Some(
                perp.time
                    .and_then(DateTime::from_timestamp_millis)
                    .ok_or_else(|| unreadable("margin reading time"))?,
            ),
        })
    }

    /// 这个标的上设置的杠杆（`activeAssetData`）。用户在交易所页面改过杠杆，下一次预检就
    /// 按新的算。
    async fn leverage(&self, instrument: &Instrument) -> Result<Leverage, ExchangeError> {
        let data: ActiveAssetData = self
            .info(
                &json!({"type": "activeAssetData", "user": self.user(), "coin": instrument.symbol()}),
                "activeAssetData",
            )
            .await?;
        if data.leverage.value == 0 {
            return Err(unreadable("leverage"));
        }
        let cross = match data.leverage.kind.as_str() {
            "cross" => true,
            "isolated" => false,
            _ => return Err(unreadable("leverage type")),
        };
        Ok(Leverage {
            value: data.leverage.value,
            cross,
        })
    }

    /// 保证金分档表（`meta` 的 `marginTables`，按标的的 `marginTableId` 找）。分档表里找不到
    /// 这张表时只有一档，按标的的 `maxLeverage` 算（测试网实测：小编号的表不在列表里）。
    async fn margin_tiers(&self, instrument: &Instrument) -> Result<MarginTiers, ExchangeError> {
        let meta: MetaResponse = self.info(&json!({"type": "meta"}), "meta").await?;
        let asset = meta
            .universe
            .iter()
            .find(|asset| asset.name == instrument.symbol())
            .ok_or_else(|| ExchangeError::Unavailable {
                reason: format!("`{instrument}` is not listed by the exchange"),
            })?;
        let table = asset.margin_table_id.and_then(|id| {
            meta.margin_tables
                .iter()
                .find(|(table_id, _)| *table_id == id)
                .map(|(_, table)| table)
        });
        match table {
            Some(table) => {
                let tiers = table
                    .margin_tiers
                    .iter()
                    .map(|tier| {
                        Ok(MarginTier {
                            lower_bound: Notional::parse(&tier.lower_bound)
                                .map_err(|_| unreadable("margin tier lower bound"))?,
                            max_leverage: tier.max_leverage,
                        })
                    })
                    .collect::<Result<Vec<_>, ExchangeError>>()?;
                MarginTiers::new(tiers).map_err(|_| unreadable("margin tier table"))
            }
            None => Ok(MarginTiers::single(
                asset
                    .max_leverage
                    .filter(|leverage| *leverage > 0)
                    .ok_or_else(|| unreadable("max leverage"))?,
            )),
        }
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

/// 读一个必须有的金额字段。
fn notional(value: Option<&str>, what: &str) -> Result<Notional, ExchangeError> {
    value
        .ok_or_else(|| unreadable(what))
        .and_then(|value| Notional::parse(value).map_err(|_| unreadable(what)))
}

fn unreadable(what: &str) -> ExchangeError {
    ExchangeError::Unavailable {
        reason: format!("the exchange returned an unreadable {what}"),
    }
}
