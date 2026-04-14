pub mod auth;
pub mod client;
mod error;
pub mod models;

pub use client::BinanceRestClient;
pub(crate) use error::BinanceRestError;
