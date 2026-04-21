use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct BinanceQuoteClient {
    client: reqwest::Client,
    base_url: String,
}

impl Default for BinanceQuoteClient {
    fn default() -> Self {
        Self::for_base_url_and_timeout("https://fapi.binance.com", DEFAULT_REQUEST_TIMEOUT)
    }
}

impl BinanceQuoteClient {
    pub fn for_base_url(base_url: impl Into<String>) -> Self {
        Self::for_base_url_and_timeout(base_url, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn for_base_url_and_timeout(base_url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("reqwest client should build"),
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
                    if error.is_timeout() {
                        QuoteErrorKind::TimedOut
                    } else {
                        QuoteErrorKind::Network
                    },
                    if error.is_timeout() {
                        "请求 Binance 合约报价超时".to_string()
                    } else {
                        format!("请求 Binance 合约报价失败: {error}")
                    },
                );
            }
        };

        let status = response.status();
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(error) => {
                return BinanceQuotePayload::failure(
                    None,
                    if error.is_timeout() {
                        QuoteErrorKind::TimedOut
                    } else {
                        QuoteErrorKind::InvalidResponse
                    },
                    if error.is_timeout() {
                        "读取 Binance 合约报价响应超时".to_string()
                    } else {
                        format!("读取 Binance 合约报价响应失败: {error}")
                    },
                );
            }
        };

        if !status.is_success() {
            return map_error_response(status, &body, symbol);
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
    RateLimited,
    TemporarilyUnavailable,
    TimedOut,
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

fn map_error_response(
    status: StatusCode,
    body: &[u8],
    requested_symbol: &str,
) -> BinanceQuotePayload {
    let error_body = serde_json::from_slice::<BinanceErrorBody>(body).ok();
    let kind = classify_error(status, error_body.as_ref());
    let error_message = build_error_message(
        kind.clone(),
        status,
        error_body.as_ref(),
        body,
        requested_symbol,
    );
    BinanceQuotePayload::failure(None, kind, error_message)
}

fn classify_error(status: StatusCode, error_body: Option<&BinanceErrorBody>) -> QuoteErrorKind {
    if status == StatusCode::BAD_REQUEST && matches!(error_body, Some(body) if body.code == -1121) {
        return QuoteErrorKind::UnsupportedSymbol;
    }
    if status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::IM_A_TEAPOT
        || matches!(error_body, Some(body) if body.code == -1003)
    {
        return QuoteErrorKind::RateLimited;
    }
    if status == StatusCode::SERVICE_UNAVAILABLE {
        return QuoteErrorKind::TemporarilyUnavailable;
    }
    QuoteErrorKind::Upstream
}

fn build_error_message(
    kind: QuoteErrorKind,
    status: StatusCode,
    error_body: Option<&BinanceErrorBody>,
    raw_body: &[u8],
    requested_symbol: &str,
) -> String {
    let upstream_message = error_body
        .map(|body| body.msg.as_str())
        .or_else(|| std::str::from_utf8(raw_body).ok())
        .map(str::trim)
        .filter(|message| !message.is_empty());

    match kind {
        QuoteErrorKind::UnsupportedSymbol => format!(
            "Binance 合约不支持 symbol `{}`: {}",
            requested_symbol.trim(),
            upstream_message.unwrap_or("unknown error")
        ),
        QuoteErrorKind::RateLimited => format!(
            "Binance 合约限流中，请稍后重试: {}",
            upstream_message.unwrap_or("unknown error")
        ),
        QuoteErrorKind::TemporarilyUnavailable => format!(
            "Binance 合约暂时不可用: {}",
            upstream_message.unwrap_or("unknown error")
        ),
        QuoteErrorKind::Upstream => format!(
            "Binance 合约报价请求失败 ({status}): {}",
            upstream_message.unwrap_or("unknown error")
        ),
        QuoteErrorKind::TimedOut | QuoteErrorKind::Network | QuoteErrorKind::InvalidResponse => {
            format!("Binance 合约报价请求失败 ({status})")
        }
    }
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
