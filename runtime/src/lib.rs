//! 运行时：账户执行，唯一写入入口。
//!
//! 只在这里读写数据库、只在这里和交易所交互。策略计算全部来自 `strategy`，
//! 这里是纯计算之外的那一层。
//!
//! 范围：多头侧、空头侧与中性网格，可选主仓、可选限速；策略生命周期、下单、成交记账、
//! 仓位核对、结果未知订单的核对与系统状态闸门、重启恢复；启动文件与私钥校验；网页用的
//! HTTP 接口与常驻服务。资金预检、保证金率、观点失效、接管属于后续任务。

pub mod account;
pub mod api;
pub mod clock;
pub mod exchange;
pub mod lock;
pub mod runtime;
pub mod secret;
pub mod serve;
pub mod settings;
pub mod simulated;
pub mod simulated_account;
pub mod startup;
pub mod store;

pub use crate::account::{Account, AccountMode, ApiWallet};
pub use crate::clock::{Clock, ManualClock, SystemClock};
pub use crate::exchange::{
    Exchange, ExchangeError, ExchangeFill, ExchangeOrder, OrderLookup, OrderReceipt, OrderRequest,
    TimeInForce,
};
pub use crate::lock::{InstanceLock, LockError};
pub use crate::runtime::{
    DAILY_ACTION_LIMIT, DraftPreview, ResumeOutcome, Runtime, RuntimeError, RuntimeOptions,
    StepReport, StrategyDetail, StrategySummary, Validation,
};
pub use crate::secret::{Address, SecretError, SecretKey};
pub use crate::serve::{ServeError, ServeOptions, Server};
pub use crate::simulated::SimulatedExchange;
pub use crate::simulated_account::SimulatedAccount;
pub use crate::startup::{
    Network, StartupCheck, StartupError, StartupFile, StartupWarning, validate as validate_startup,
};
pub use crate::store::{
    DraftId, Store, StoreError, StoredConfigVersion, StoredDraft, StoredFill, StoredOrder,
    StoredStrategy, StrategyId, StrategyStatus, ViewSnapshot,
};
