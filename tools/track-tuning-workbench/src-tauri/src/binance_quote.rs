use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct BinanceQuoteClient {
    client: reqwest::Client,
    base_url: String,
}

impl Default for BinanceQuoteClient {
    fn default() -> Self {
        Self::for_base_url("https://fapi.binance.com")
    }
}

impl BinanceQuoteClient {
    pub fn for_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn fetch_quote(&self, symbol: &str) -> BinanceQuotePayload {
        let normalized_symbol = symbol.trim().to_ascii_uppercase();
        if normalized_symbol.is_empty() {
            return BinanceQuotePayload::failure(
                None,
                QuoteErrorKind::UnsupportedSymbol,
                "symbol 不能为空".to_string(),
            );
        }

        let url = format!("{}/fapi/v1/ticker/price", self.base_url);
        let response = match self
            .client
            .get(url)
            .query(&[("symbol", normalized_symbol.as_str())])
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return BinanceQuotePayload::failure(
                    None,
                    QuoteErrorKind::Network,
                    format!("请求 Binance 合约报价失败: {error}"),
                );
            }
        };

        let status = response.status();
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                return BinanceQuotePayload::failure(
                    None,
                    QuoteErrorKind::InvalidResponse,
                    format!("读取 Binance 合约报价响应失败: {error}"),
                );
            }
        };

        if status == StatusCode::BAD_REQUEST {
            if let Ok(error_body) = serde_json::from_slice::<BinanceErrorBody>(&body) {
                if error_body.code == -1121 {
                    return BinanceQuotePayload::failure(
                        None,
                        QuoteErrorKind::UnsupportedSymbol,
                        format!(
                            "Binance 合约不支持 symbol `{}`: {}",
                            symbol.trim(),
                            error_body.msg
                        ),
                    );
                }
            }
        }

        if !status.is_success() {
            return BinanceQuotePayload::failure(
                None,
                QuoteErrorKind::Upstream,
                format!("Binance 合约报价请求失败，状态码 {status}"),
            );
        }

        match serde_json::from_slice::<BinanceTickerPriceBody>(&body) {
            Ok(payload) => BinanceQuotePayload::success(payload.price),
            Err(error) => BinanceQuotePayload::failure(
                None,
                QuoteErrorKind::InvalidResponse,
                format!("解析 Binance 合约报价失败: {error}"),
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuoteErrorKind {
    UnsupportedSymbol,
    Network,
    Upstream,
    InvalidResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BinanceQuotePayload {
    pub price: Option<String>,
    pub retrieved_at: u64,
    pub error_kind: Option<QuoteErrorKind>,
    pub error_message: Option<String>,
}

impl BinanceQuotePayload {
    fn success(price: String) -> Self {
        Self {
            price: Some(price),
            retrieved_at: unix_timestamp_ms(),
            error_kind: None,
            error_message: None,
        }
    }

    fn failure(price: Option<String>, error_kind: QuoteErrorKind, error_message: String) -> Self {
        Self {
            price,
            retrieved_at: unix_timestamp_ms(),
            error_kind: Some(error_kind),
            error_message: Some(error_message),
        }
    }
}

#[derive(Debug, Deserialize)]
struct BinanceTickerPriceBody {
    price: String,
}

#[derive(Debug, Deserialize)]
struct BinanceErrorBody {
    code: i64,
    msg: String,
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
