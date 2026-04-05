//! Binance 交易所适配 crate 只公开稳定门面。
//!
//! ```rust
//! use poise_binance::BinanceAdapter;
//! ```

mod adapter;
mod rest;
mod types;
mod websocket;

pub use adapter::BinanceAdapter;
