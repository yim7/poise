//! 账户级查询接口。
//!
//! [`crate::exchange::Exchange`] 都是按标的问的；这里是不分标的的那部分：账户模式、
//! 已授权的 API 钱包、可用资金、费率和合约规则。启动校验和资金预检用它，
//! 由交易所适配实现（Hyperliquid 在 `next/hyperliquid`）。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::units::{Notional, Rate, SCALE};

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

/// 账户保证金读数：Unified 账户比率的组成部分（第 7.4 节）。
///
/// 官方的账户比率按每种抵押资产算「全仓维持保证金占用 ÷（余额 − 逐仓保证金）」，取最大值，
/// 要汇总所有合约 DEX。本期只有 USDC 一种抵押资产，DEX 却可能有几百个（HIP-3），逐个查询
/// 不现实；所以默认 DEX 按官方公式算，其他 DEX 的占用从现货账户状态里的「维持保证金之后的
/// 可用量」推出来（统一账户的全部余额与占用都汇总在那里），两者取较大的那个（[`Self::ratio`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginReading {
    /// USDC 余额（现货账户状态里的总额）。
    pub balance: Notional,
    /// USDC 在维持保证金之后的可用量（现货账户状态，覆盖整个账户）。
    pub available_after_maintenance: Notional,
    /// 默认 DEX 上全仓仓位的维持保证金占用合计。
    pub cross_maintenance: Notional,
    /// 默认 DEX 上逐仓仓位占用的保证金合计。
    pub isolated_margin: Notional,
    /// 交易所给出这份读数的时刻，调用方据此判断读数有没有过期。真实交易所的适配必须给出；
    /// 给不出时报「读不到」。`None` 只留给不经网络的实现（模拟账户），按读到的那一刻算。
    pub as_of: Option<DateTime<Utc>>,
}

impl MarginReading {
    /// 账户比率：默认 DEX 按官方公式算的比率，和按整个账户的占用推出的比率，取较大的那个。
    ///
    /// 整个账户的占用 = 余额 − 维持保证金之后的可用量。测试网实测，只有全仓仓位时它正好等于
    /// 全仓维持保证金占用；其他 DEX 上的仓位也会算进来，不会被漏掉。
    ///
    /// 分母（余额 − 逐仓保证金）不为正时按 100% 算：这时账户已经没有余地，宁可高估。
    pub fn ratio(&self) -> Rate {
        let free = i128::from(self.balance.scaled_floor())
            - i128::from(self.isolated_margin.scaled_floor());
        if free <= 0 {
            return Rate::from_units(SCALE);
        }
        let cross = i128::from(self.cross_maintenance.scaled_floor()).max(0);
        let account_wide = (i128::from(self.balance.scaled_floor())
            - i128::from(self.available_after_maintenance.scaled_floor()))
        .max(0);
        let used = cross.max(account_wide);
        let units = (used * i128::from(SCALE) / free).min(i128::from(i64::MAX));
        Rate::from_units(units as i64)
    }
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

    /// 账户保证金读数，用来算账户比率（第 7.4 节）。
    async fn margin(&self) -> Result<MarginReading, ExchangeError>;

    /// 账户实际费率。
    async fn fees(&self) -> Result<Fees, ExchangeError>;

    /// 一个标的的合约规则：报价精度、数量精度、最低订单金额。
    async fn instrument_spec(
        &self,
        instrument: &Instrument,
    ) -> Result<InstrumentSpec, ExchangeError>;
}

#[cfg(test)]
mod tests {
    use super::MarginReading;
    use poise_strategy::units::{Notional, Rate};

    /// 只有默认 DEX 有占用的读数：可用量 = 余额 − 全仓维持保证金。
    fn reading(balance: &str, cross: &str, isolated: &str) -> MarginReading {
        let balance = Notional::parse(balance).unwrap();
        let cross_maintenance = Notional::parse(cross).unwrap();
        MarginReading {
            balance,
            available_after_maintenance: Notional::from_scaled(
                balance.scaled_floor() - cross_maintenance.scaled_floor(),
            ),
            cross_maintenance,
            isolated_margin: Notional::parse(isolated).unwrap(),
            as_of: None,
        }
    }

    #[test]
    fn the_ratio_is_cross_maintenance_over_free_balance() {
        // 余额 1000，逐仓占用 200，全仓维持保证金 200：200 ÷ 800 = 25%。
        assert_eq!(
            reading("1000", "200", "200").ratio(),
            Rate::parse("0.25").unwrap()
        );
    }

    #[test]
    fn no_cross_positions_means_a_zero_ratio() {
        assert_eq!(reading("1000", "0", "0").ratio(), Rate::ZERO);
    }

    #[test]
    fn a_balance_eaten_by_isolated_margin_counts_as_full() {
        assert_eq!(
            reading("100", "5", "100").ratio(),
            Rate::parse("1").unwrap()
        );
        // 没有全仓占用也一样：分母先判断，不能因为分子是 0 就报 0%。
        assert_eq!(
            reading("100", "0", "100").ratio(),
            Rate::parse("1").unwrap()
        );
        assert_eq!(
            reading("100", "0", "150").ratio(),
            Rate::parse("1").unwrap()
        );
    }

    /// 别的 DEX 上的占用只体现在整个账户的可用量里：按它推出的比率更高时取它。
    #[test]
    fn margin_used_on_other_dexes_raises_the_ratio() {
        let mut margin = reading("1000", "100", "0");
        // 默认 DEX 占用 100，别的 DEX 又占了 500：可用量只剩 400。
        margin.available_after_maintenance = Notional::parse("400").unwrap();

        assert_eq!(margin.ratio(), Rate::parse("0.6").unwrap());
    }
}
