//! 运行时：账户执行，唯一写入入口。
//!
//! 只在这里读写数据库、只在这里和交易所交互。策略计算全部来自 `strategy`，
//! 这里是纯计算之外的那一层。
//!
//! 本任务范围：做多网格、没有主仓、不限速；创建策略、下单、成交记账、
//! 仓位核对、重启恢复。HTTP 接口、Hyperliquid 适配、资金预检属于后续任务。

pub mod exchange;
pub mod runtime;
pub mod simulated;
pub mod store;

pub use crate::exchange::{
    Exchange, ExchangeError, ExchangeFill, ExchangeOrder, OrderLookup, OrderReceipt, OrderRequest,
};
pub use crate::runtime::{ResumeOutcome, Runtime, RuntimeError, RuntimeOptions, StepReport};
pub use crate::simulated::SimulatedExchange;
pub use crate::store::{
    Store, StoreError, StoredFill, StoredOrder, StoredStrategy, StrategyId, StrategyStatus,
};
