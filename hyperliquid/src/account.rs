//! [`Account`] 的 Hyperliquid 实现：账户模式、API 钱包、可用资金、费率、合约规则。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_runtime::account::{
    FUNDING_HOURS_PER_YEAR, FundingPayment, FundingRate, Leverage, MarginReading,
};
use poise_runtime::{Account, AccountMode, ApiWallet, ExchangeError};
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::stress::{MarginTier, MarginTiers};
use poise_strategy::units::{Notional, Rate};
use serde_json::{Value, json};

use crate::client::Hyperliquid;
use crate::models::{
    ActiveAssetData, AgentResponse, ClearinghouseState, FundingEntry, MetaResponse,
    SpotClearinghouseState, UserFees,
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

    async fn funding_rate(&self, instrument: &Instrument) -> Result<FundingRate, ExchangeError> {
        let context = self.asset_context(instrument).await?;
        let funding = context.funding.ok_or_else(|| unreadable("funding rate"))?;
        funding_rate(&funding).ok_or_else(|| unreadable("funding rate"))
    }

    /// `userFunding` 一次最多返回 500 条，下一页从上一页最后一条的时刻（含）接着查，所以
    /// 页与页之间会重一条，按时刻去重。返回的是整个账户所有标的的结算，只留这个标的的。
    async fn funding_payments(
        &self,
        instrument: &Instrument,
        since: DateTime<Utc>,
    ) -> Result<Vec<FundingPayment>, ExchangeError> {
        let mut payments: Vec<FundingPayment> = Vec::new();
        let mut start = since.timestamp_millis();
        for _ in 0..FUNDING_PAGES {
            let body = json!({"type": "userFunding", "user": self.user(), "startTime": start});
            let page: Vec<FundingEntry> = self.info(&body, "userFunding").await?;
            let Some(last) = page.last().map(|entry| entry.time) else {
                return Ok(payments);
            };
            for entry in &page {
                if entry.delta.coin != instrument.symbol() {
                    continue;
                }
                let time = DateTime::from_timestamp_millis(entry.time)
                    .ok_or_else(|| unreadable("funding time"))?;
                if payments.iter().any(|payment| payment.time == time) {
                    continue;
                }
                payments.push(FundingPayment {
                    time,
                    amount: Notional::parse(&entry.delta.usdc)
                        .map_err(|_| unreadable("funding amount"))?,
                });
            }
            // 不满一页就是查完了。满页、最后一条的时刻却没往前走：同一时刻的记录多于一页，
            // 后面的翻不到，不能当成查完了——报读不全，调用方不推进同步点。
            if page.len() < FUNDING_PAGE_SIZE {
                return Ok(payments);
            }
            if last <= start {
                return Err(ExchangeError::Unavailable {
                    reason: "a full page of funding payments at one time; the rest cannot be paged"
                        .to_string(),
                });
            }
            start = last;
        }
        Err(ExchangeError::Unavailable {
            reason: format!("more than {FUNDING_PAGES} pages of funding payments"),
        })
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

/// `userFunding` 一页的条数上限（文档「Pagination」：按时间范围查的接口最多返回 500 条）。
const FUNDING_PAGE_SIZE: usize = 500;

/// 一次最多翻几页：每页 500 条、每小时结算一次，100 页够查好几年的单个账户全部标的。
const FUNDING_PAGES: usize = 100;

/// 交易所给的每小时资金费率（可能多于 8 位小数）换成 [`FundingRate`]。
///
/// 年化按原始精度算再取到 8 位，判断阈值用它：先截断每小时费率再乘 8760，在阈值附近会
/// 漏掉提醒（每小时 0.0000570799 年化 50.0019924%，截断后只有 49.99332%）。每小时费率只
/// 用来显示，截到 8 位。两者都向零取整。
fn funding_rate(value: &str) -> Option<FundingRate> {
    let value = value.trim();
    let (negative, digits) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let (integer, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if integer.is_empty() && fraction.is_empty()
        || !integer
            .chars()
            .chain(fraction.chars())
            .all(|c| c.is_ascii_digit())
        || fraction.len() > 18
    {
        return None;
    }
    let scale = 10_i128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let mantissa: i128 = format!("{integer}{fraction}").parse().ok()?;
    let mantissa = if negative { -mantissa } else { mantissa };
    let to_units = |times: i128| -> Option<Rate> {
        let units = mantissa
            .checked_mul(times)?
            .checked_mul(i128::from(poise_strategy::units::SCALE))?
            / scale;
        Some(Rate::from_units(i64::try_from(units).ok()?))
    };
    FundingRate::new(to_units(1)?, to_units(i128::from(FUNDING_HOURS_PER_YEAR))?)
}

#[cfg(test)]
mod tests {
    use super::funding_rate;
    use poise_strategy::units::Rate;

    fn rate(value: &str) -> Rate {
        Rate::parse(value).unwrap()
    }

    /// 年化按原始精度算：截断之后再乘会在阈值附近漏掉提醒。正负两个方向都一样。
    #[test]
    fn the_annual_funding_rate_keeps_the_full_precision() {
        let positive = funding_rate("0.0000570799").unwrap();
        assert_eq!(positive.hourly, rate("0.00005707"));
        assert_eq!(positive.annual, rate("0.50001992"));
        assert!(positive.annual > rate("0.5"));

        let negative = funding_rate(" -0.0000570799").unwrap();
        assert_eq!(negative.hourly, rate("-0.00005707"));
        assert_eq!(negative.annual, rate("-0.50001992"));
    }

    #[test]
    fn an_unreadable_funding_rate_is_none() {
        for value in ["", "-", "abc", "1e-5", "0.0000000000000000001"] {
            assert_eq!(funding_rate(value), None, "{value}");
        }
        // 超出能表示的范围也读不出来，不能回绕成别的数。
        assert_eq!(funding_rate("99999999999999"), None);
        // 年化正好是 i64::MIN：取反会溢出，同样读不出来。
        assert_eq!(funding_rate("-10528963.512391296584474886"), None);
    }
}
