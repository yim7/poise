use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct BinanceQuoteClient {
    client: reqwest::Client,
    binance_base_url: String,
    okx_base_url: String,
}

impl Default for BinanceQuoteClient {
    fn default() -> Self {
        Self::for_base_urls_and_timeout(
            "https://fapi.binance.com",
            "https://www.okx.com",
            DEFAULT_REQUEST_TIMEOUT,
        )
    }
}

impl BinanceQuoteClient {
    pub fn for_base_url(base_url: impl Into<String>) -> Self {
        Self::for_base_url_and_timeout(base_url, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn for_base_url_and_timeout(base_url: impl Into<String>, timeout: Duration) -> Self {
        let base_url = base_url.into();
        Self::for_base_urls_and_timeout(base_url.clone(), base_url, timeout)
    }

    pub fn for_base_urls_and_timeout(
        binance_base_url: impl Into<String>,
        okx_base_url: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("reqwest client should build"),
            binance_base_url: binance_base_url.into().trim_end_matches('/').to_string(),
            okx_base_url: okx_base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub async fn fetch_quote(&self, symbol: &str) -> BinanceQuotePayload {
        self.fetch_quote_for_exchange(Some("binance"), symbol).await
    }

    pub async fn fetch_quote_for_exchange(
        &self,
        exchange_venue: Option<&str>,
        symbol: &str,
    ) -> BinanceQuotePayload {
        if normalized_exchange_venue(exchange_venue).as_str() == "okx" {
            return self.fetch_okx_quote(symbol).await;
        }

        let normalized_symbol = match translate_to_binance_futures_symbol(exchange_venue, symbol) {
            Ok(symbol) => symbol,
            Err(message) => {
                return BinanceQuotePayload::failure(
                    None,
                    QuoteErrorKind::UnsupportedSymbol,
                    message,
                );
            }
        };

        let url = format!("{}/fapi/v1/ticker/price", self.binance_base_url);
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

    async fn fetch_okx_quote(&self, symbol: &str) -> BinanceQuotePayload {
        let symbol = symbol.trim();
        if symbol.is_empty() {
            return BinanceQuotePayload::failure(
                None,
                QuoteErrorKind::UnsupportedSymbol,
                "symbol 不能为空".to_string(),
            );
        }

        let url = format!("{}/api/v5/market/ticker", self.okx_base_url);
        let response = match self
            .client
            .get(url)
            .query(&[("instId", symbol)])
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
                        "请求 OKX 合约报价超时".to_string()
                    } else {
                        format!("请求 OKX 合约报价失败: {error}")
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
                        "读取 OKX 合约报价响应超时".to_string()
                    } else {
                        format!("读取 OKX 合约报价响应失败: {error}")
                    },
                );
            }
        };

        if !status.is_success() {
            return BinanceQuotePayload::failure(
                None,
                QuoteErrorKind::Upstream,
                format!("OKX 合约报价请求失败 ({status})"),
            );
        }

        let payload = match serde_json::from_slice::<OkxTickerEnvelope>(&body) {
            Ok(payload) => payload,
            Err(error) => {
                return BinanceQuotePayload::failure(
                    None,
                    QuoteErrorKind::InvalidResponse,
                    format!("解析 OKX 合约报价失败: {error}"),
                );
            }
        };

        if payload.code != "0" {
            let kind = if payload.code == "51001" {
                QuoteErrorKind::UnsupportedSymbol
            } else {
                QuoteErrorKind::Upstream
            };
            return BinanceQuotePayload::failure(
                None,
                kind,
                format!(
                    "OKX 合约报价请求失败 code {}: {}",
                    payload.code,
                    payload.msg.unwrap_or_else(|| "unknown error".to_string())
                ),
            );
        }

        match payload.data.into_iter().next() {
            Some(ticker) => BinanceQuotePayload::success(ticker.last),
            None => BinanceQuotePayload::failure(
                None,
                QuoteErrorKind::UnsupportedSymbol,
                format!("OKX 合约不支持 symbol `{symbol}`"),
            ),
        }
    }
}

fn normalized_exchange_venue(exchange_venue: Option<&str>) -> String {
    exchange_venue
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("binance")
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn translate_to_binance_futures_symbol(
    exchange_venue: Option<&str>,
    symbol: &str,
) -> Result<String, String> {
    let normalized_venue = normalized_exchange_venue(exchange_venue);
    let raw_symbol = symbol.trim();

    if raw_symbol.is_empty() {
        return Err("symbol 不能为空".to_string());
    }

    match normalized_venue.as_str() {
        "binance" | "binance_futures" | "binance_usds_futures" | "bybit" | "hyperliquid" => {
            translate_delimited_or_bare_symbol(raw_symbol)
        }
        "okx" => translate_okx_symbol(raw_symbol),
        other => Err(format!(
            "暂不支持将交易所 `{other}` 的 symbol 转成 Binance 合约格式"
        )),
    }
}

fn translate_okx_symbol(symbol: &str) -> Result<String, String> {
    let parts = split_symbol_parts(symbol);
    if parts.len() >= 2 && parts.last().is_some_and(|value| value == "SWAP") {
        return Ok(format!("{}{}", parts[0], parts[1]));
    }
    translate_delimited_or_bare_symbol(symbol)
}

fn translate_delimited_or_bare_symbol(symbol: &str) -> Result<String, String> {
    let parts = split_symbol_parts(symbol);
    if parts.is_empty() {
        return Err("symbol 不能为空".to_string());
    }

    if parts.len() >= 2 && is_quote_asset(&parts[1]) {
        return Ok(format!("{}{}", parts[0], parts[1]));
    }

    let collapsed = parts.join("");
    if has_known_quote_suffix(&collapsed) {
        return Ok(collapsed);
    }

    Ok(format!("{collapsed}USDT"))
}

fn split_symbol_parts(symbol: &str) -> Vec<String> {
    symbol
        .trim()
        .to_ascii_uppercase()
        .split(['-', '_', '/', ':'])
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn has_known_quote_suffix(symbol: &str) -> bool {
    known_quote_assets()
        .iter()
        .any(|quote| symbol.len() > quote.len() && symbol.ends_with(quote))
}

fn is_quote_asset(value: &str) -> bool {
    known_quote_assets().contains(&value)
}

fn known_quote_assets() -> [&'static str; 4] {
    ["USDT", "USDC", "BUSD", "USD"]
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
struct OkxTickerEnvelope {
    code: String,
    msg: Option<String>,
    data: Vec<OkxTickerBody>,
}

#[derive(Debug, Deserialize)]
struct OkxTickerBody {
    last: String,
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
