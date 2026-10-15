//! 模拟账户。
//!
//! 只用于测试：不连接任何外部服务，账户模式、已授权的 API 钱包、可用资金、费率和
//! 合约规则都由测试直接给出，也由测试随时改动（用来构造「预览之后账户变了」）。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::units::Notional;

use crate::account::{Account, AccountMode, ApiWallet};
use crate::exchange::ExchangeError;

/// 默认的可用抵押：足够大，不会意外挡住配置。
const DEFAULT_COLLATERAL: &str = "100000";

pub struct SimulatedAccount {
    state: Mutex<SimAccount>,
}

struct SimAccount {
    mode: AccountMode,
    wallets: Vec<ApiWallet>,
    collateral: Notional,
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
        Ok(self.lock().collateral)
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
