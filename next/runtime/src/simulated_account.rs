//! 模拟账户。
//!
//! 只用于测试：不连接任何外部服务，账户模式、已授权的 API 钱包、可用资金、费率和
//! 合约规则都由测试直接给出，也由测试随时改动（用来构造「预览之后账户变了」）。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::stress::MarginTiers;
use poise_strategy::units::Notional;

use crate::account::{Account, AccountMode, ApiWallet, Leverage, MarginReading};
use crate::exchange::ExchangeError;

/// 默认的可用抵押：足够大，不会意外挡住配置。
const DEFAULT_COLLATERAL: &str = "100000";

/// 默认的杠杆倍数。
const DEFAULT_LEVERAGE: u32 = 10;

pub struct SimulatedAccount {
    state: Mutex<SimAccount>,
}

struct SimAccount {
    mode: AccountMode,
    wallets: Vec<ApiWallet>,
    collateral: Notional,
    /// 下一次查可用资金失败的原因。可用资金只用于展示，拿不到它不该挡住撤单。
    fail_next_collateral: Option<String>,
    /// 保证金读数：默认没有任何占用，账户比率为 0。
    margin: MarginReading,
    /// 还要让几次保证金读数失败。
    fail_margin: usize,
    /// 杠杆设置；默认 [`DEFAULT_LEVERAGE`] 倍全仓。
    leverage: Leverage,
    /// 还要让几次杠杆读数失败。
    fail_leverage: usize,
    /// 保证金分档表：默认只有一档 40 倍。
    margin_tiers: MarginTiers,
    fees: Fees,
    /// 各标的的合约规则，按标的符号索引。
    specs: HashMap<String, InstrumentSpec>,
}

impl SimulatedAccount {
    /// 一个 Unified 账户，带一个标的的合约规则和一组费率。
    pub fn new(spec: InstrumentSpec, fees: Fees) -> Self {
        let mut specs = HashMap::new();
        specs.insert(spec.instrument().symbol().to_string(), spec);
        Self {
            state: Mutex::new(SimAccount {
                mode: AccountMode::Unified,
                wallets: Vec::new(),
                collateral: Notional::parse(DEFAULT_COLLATERAL).expect("a valid default"),
                fail_next_collateral: None,
                margin: MarginReading {
                    balance: Notional::parse(DEFAULT_COLLATERAL).expect("a valid default"),
                    available_after_maintenance: Notional::parse(DEFAULT_COLLATERAL)
                        .expect("a valid default"),
                    cross_maintenance: Notional::ZERO,
                    isolated_margin: Notional::ZERO,
                    as_of: None,
                },
                fail_margin: 0,
                leverage: Leverage {
                    value: DEFAULT_LEVERAGE,
                    cross: true,
                },
                fail_leverage: 0,
                margin_tiers: MarginTiers::single(40),
                fees,
                specs,
            }),
        }
    }

    /// 再加一个标的的合约规则。
    pub fn with_instrument(self, spec: InstrumentSpec) -> Self {
        self.lock()
            .specs
            .insert(spec.instrument().symbol().to_string(), spec);
        self
    }

    pub fn with_mode(self, mode: AccountMode) -> Self {
        self.lock().mode = mode;
        self
    }

    pub fn with_api_wallet(self, wallet: ApiWallet) -> Self {
        self.lock().wallets.push(wallet);
        self
    }

    pub fn with_collateral(self, collateral: Notional) -> Self {
        self.lock().collateral = collateral;
        self
    }

    /// 可用抵押变了（例如用户在现货买入）。
    pub fn set_collateral(&self, collateral: Notional) {
        self.lock().collateral = collateral;
    }

    /// 让下一次查可用资金失败：账户接口暂时不可用。
    pub fn fail_next_collateral(&self, reason: &str) {
        self.lock().fail_next_collateral = Some(reason.to_string());
    }

    /// 保证金读数变了（例如用户在别的标的上加了仓）。
    pub fn set_margin(&self, margin: MarginReading) {
        self.lock().margin = margin;
    }

    /// 让接下来 `count` 次保证金读数失败：账户接口暂时不可用。
    pub fn fail_margin(&self, count: usize) {
        self.lock().fail_margin = count;
    }

    /// 让接下来 `count` 次杠杆读数失败。
    pub fn fail_leverage(&self, count: usize) {
        self.lock().fail_leverage = count;
    }

    /// 换一张保证金分档表。
    pub fn set_margin_tiers(&self, tiers: MarginTiers) {
        self.lock().margin_tiers = tiers;
    }

    /// 杠杆设置变了（倍数或者全仓、逐仓）。
    pub fn set_leverage(&self, leverage: Leverage) {
        self.lock().leverage = leverage;
    }

    /// 费率变了。
    pub fn set_fees(&self, fees: Fees) {
        self.lock().fees = fees;
    }

    pub fn set_mode(&self, mode: AccountMode) {
        self.lock().mode = mode;
    }

    pub fn set_api_wallets(&self, wallets: Vec<ApiWallet>) {
        self.lock().wallets = wallets;
    }

    fn lock(&self) -> MutexGuard<'_, SimAccount> {
        self.state.lock().expect("simulated account lock poisoned")
    }
}

#[async_trait]
impl Account for SimulatedAccount {
    async fn mode(&self) -> Result<AccountMode, ExchangeError> {
        Ok(self.lock().mode.clone())
    }

    async fn authorized_api_wallets(&self) -> Result<Vec<ApiWallet>, ExchangeError> {
        Ok(self.lock().wallets.clone())
    }

    async fn available_collateral(&self) -> Result<Notional, ExchangeError> {
        let mut state = self.lock();
        if let Some(reason) = state.fail_next_collateral.take() {
            return Err(ExchangeError::Unavailable { reason });
        }
        Ok(state.collateral)
    }

    async fn margin(&self) -> Result<MarginReading, ExchangeError> {
        let mut state = self.lock();
        if state.fail_margin > 0 {
            state.fail_margin -= 1;
            return Err(ExchangeError::Unavailable {
                reason: "the margin summary is unavailable".to_string(),
            });
        }
        Ok(state.margin)
    }

    async fn leverage(&self, _instrument: &Instrument) -> Result<Leverage, ExchangeError> {
        let mut state = self.lock();
        if state.fail_leverage > 0 {
            state.fail_leverage -= 1;
            return Err(ExchangeError::Unavailable {
                reason: "the leverage is unavailable".to_string(),
            });
        }
        Ok(state.leverage)
    }

    async fn margin_tiers(&self, _instrument: &Instrument) -> Result<MarginTiers, ExchangeError> {
        Ok(self.lock().margin_tiers.clone())
    }

    async fn fees(&self) -> Result<Fees, ExchangeError> {
        Ok(self.lock().fees)
    }

    async fn instrument_spec(
        &self,
        instrument: &Instrument,
    ) -> Result<InstrumentSpec, ExchangeError> {
        self.lock()
            .specs
            .get(instrument.symbol())
            .cloned()
            .ok_or_else(|| ExchangeError::Unavailable {
                reason: format!("no contract rules for `{instrument}`"),
            })
    }
}
