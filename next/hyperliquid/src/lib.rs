//! Hyperliquid 适配。
//!
//! 把 [`poise_runtime::Exchange`] 和 [`poise_runtime::Account`] 落到 Hyperliquid 的
//! REST 接口上：`/info` 读、`/exchange` 写（签名见 [`signing`]）。
//!
//! 几条贯穿全 crate 的原则：
//!
//! - **数不清就说不清**：交易所返回的数字或状态看不懂时，写接口报「结果未知」，
//!   读接口失败。运行时会保住预留、不重发，宁可停下来也不重复下单。
//! - **本地不合规就不发**：报价或数量不符合交易所规则时本地拒绝，不改数。
//! - **不猜累计成交量**：交易所不给原始数量时按 0 算，靠仓位核对兜底。
//!
//! 只有永续合约、只做默认的 perp dex（HIP-3 的 `dex:COIN` 标的属于后续任务）。

mod account;
mod actions;
mod client;
mod exchange;
mod ids;
mod meta;
mod models;
mod signing;
mod transport;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use client::{Hyperliquid, HyperliquidConfig};
