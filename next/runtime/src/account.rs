//! 账户级查询接口。
//!
//! [`crate::exchange::Exchange`] 都是按标的问的；这里是不分标的的那部分：账户模式、
//! 已授权的 API 钱包、可用资金、费率和合约规则。启动校验和资金预检用它，
//! 由交易所适配实现（Hyperliquid 在 `next/hyperliquid`）。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::units::Notional;

use crate::exchange::ExchangeError;
use crate::secret::Address;

/// 账户模式（Hyperliquid `userAbstraction`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountMode {
    /// `unifiedAccount`：本期唯一支持的模式（设计第 10 节）。
    Unified,
    /// 其他模式，保留交易所给的原文用于报错。
    Other(String),
}

impl AccountMode {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unified => "unifiedAccount",
            Self::Other(mode) => mode,
        }
    }
}

/// 一个已授权的 API 钱包（Hyperliquid `extraAgents` 的一项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiWallet {
    pub name: String,
    pub address: Address,
    /// 授权到期时间。
    pub valid_until: DateTime<Utc>,
}

#[async_trait]
pub trait Account: Send + Sync {
    /// 账户模式。不是 Unified 时启动校验会拒绝启动，不替用户切换。
    async fn mode(&self) -> Result<AccountMode, ExchangeError>;

    /// 这个账户当前已授权、且尚未被替换的 API 钱包。
    async fn authorized_api_wallets(&self) -> Result<Vec<ApiWallet>, ExchangeError>;

    /// 可用抵押（Unified 账户下是 USDC 在维持保证金之后的可用量）。
    ///
    /// 不使用可提现金额（设计第 10 节）。
    async fn available_collateral(&self) -> Result<Notional, ExchangeError>;

    /// 账户实际费率。
    async fn fees(&self) -> Result<Fees, ExchangeError>;

    /// 一个标的的合约规则：报价精度、数量精度、最低订单金额。
    async fn instrument_spec(
        &self,
        instrument: &Instrument,
    ) -> Result<InstrumentSpec, ExchangeError>;
}
