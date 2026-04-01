use std::net::IpAddr;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};
use hmac::{Hmac, Mac};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use sha2::Sha256;
use tokio::time::{Duration, sleep};
use url::{Host, Url, form_urlencoded::Serializer};

use poise_engine::ports::{
    AccountMarginSnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt, OrderRequest, Position,
};

use crate::types::{
    BinanceAccountInformation, BinanceExchangeInfoResponse, BinanceOpenOrder,
    BinanceOrderResponse, BinancePositionRisk,
};

const DEFAULT_RECV_WINDOW_MS: i64 = 10_000;
const MAX_RETRIES: usize = 3;
const MAX_DECIMAL_SCALE: u32 = 16;
const SIGNED_TIME_SYNC_REFRESH_INTERVAL_MS: i64 = 60_000;

#[derive(Debug, Clone, Copy)]
enum AuthMode {
    None,
    ApiKey,
    Signed,
}

pub struct BinanceRestClient {
    http: reqwest::Client,
    api_key: String,
    api_secret: String,
    base_url: String,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    timestamp_offset_ms: AtomicI64,
    last_time_sync_at_ms: AtomicI64,
}

impl BinanceRestClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http: build_http_client(&base_url),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            base_url,
            timestamp_provider: Arc::new(|| chrono::Utc::now().timestamp_millis()),
            timestamp_offset_ms: AtomicI64::new(0),
            last_time_sync_at_ms: AtomicI64::new(0),
        }
    }

    #[cfg(test)]
    fn with_timestamp_provider(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http: build_http_client(&base_url),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            base_url,
            timestamp_provider,
            timestamp_offset_ms: AtomicI64::new(0),
            last_time_sync_at_ms: AtomicI64::new(0),
        }
    }

    #[cfg(test)]
    fn with_http_client_and_timestamp_provider(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
        http: reqwest::Client,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http,
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            base_url,
            timestamp_provider,
            timestamp_offset_ms: AtomicI64::new(0),
            last_time_sync_at_ms: AtomicI64::new(0),
        }
    }

    pub fn sign_query(&self, query: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC-SHA256 accepts any key length");
        mac.update(query.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    pub async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo> {
        let response: BinanceExchangeInfoResponse = self
            .send_request(
                Method::GET,
                "/fapi/v1/exchangeInfo",
                Vec::new(),
                AuthMode::None,
            )
            .await?;

        let symbol_info = response
            .symbols
            .into_iter()
            .find(|item| item.symbol == symbol)
            .with_context(|| format!("symbol not found in exchange info: {symbol}"))?;

        symbol_info.try_into()
    }

    pub async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        self.sync_server_time_offset().await
    }

    pub async fn get_position(&self, symbol: &str) -> Result<Position> {
        let positions: Vec<BinancePositionRisk> = self
            .send_request(
                Method::GET,
                "/fapi/v2/positionRisk",
                vec![("symbol", symbol.to_string())],
                AuthMode::Signed,
            )
            .await?;

        let position = positions
            .into_iter()
            .find(|item| item.symbol == symbol)
            .with_context(|| format!("position not found for symbol: {symbol}"))?;

        position.try_into()
    }

    pub async fn get_open_orders(&self, symbol: &str) -> Result<Vec<ExchangeOrder>> {
        let orders: Vec<BinanceOpenOrder> = self
            .send_request(
                Method::GET,
                "/fapi/v1/openOrders",
                vec![("symbol", symbol.to_string())],
                AuthMode::Signed,
            )
            .await?;

        orders
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>>>()
    }

    pub async fn get_account_margin_snapshot(&self, symbol: &str) -> Result<AccountMarginSnapshot> {
        let account: BinanceAccountInformation = self
            .send_request(Method::GET, "/fapi/v2/account", Vec::new(), AuthMode::Signed)
            .await?;

        account.into_margin_snapshot(symbol)
    }

    pub async fn new_order(&self, req: &OrderRequest) -> Result<OrderReceipt> {
        let mut params = vec![
            ("symbol", req.instrument.symbol.clone()),
            ("side", side_to_binance(req.side).to_string()),
            ("type", "LIMIT".to_string()),
            ("timeInForce", "GTC".to_string()),
            ("quantity", format_decimal(req.quantity)),
            ("price", format_decimal(req.price)),
            ("newClientOrderId", req.client_order_id.clone()),
        ];
        if req.reduce_only {
            params.push(("reduceOnly", "true".to_string()));
        }
        let response: BinanceOrderResponse = self
            .send_request(Method::POST, "/fapi/v1/order", params, AuthMode::Signed)
            .await?;

        response.try_into()
    }

    pub async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<OrderReceipt> {
        let response: BinanceOrderResponse = self
            .send_request(
                Method::DELETE,
                "/fapi/v1/order",
                vec![
                    ("symbol", symbol.to_string()),
                    ("orderId", order_id.to_string()),
                ],
                AuthMode::Signed,
            )
            .await?;

        response.try_into()
    }

    pub async fn cancel_all_orders(&self, symbol: &str) -> Result<()> {
        let _: serde_json::Value = self
            .send_request(
                Method::DELETE,
                "/fapi/v1/allOpenOrders",
                vec![("symbol", symbol.to_string())],
                AuthMode::Signed,
            )
            .await?;
        Ok(())
    }

    pub async fn start_user_stream(&self) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct ListenKeyResponse {
            #[serde(rename = "listenKey")]
            listen_key: String,
        }

        let response: ListenKeyResponse = self
            .send_request(
                Method::POST,
                "/fapi/v1/listenKey",
                Vec::new(),
                AuthMode::ApiKey,
            )
            .await?;

        Ok(response.listen_key)
    }

    pub async fn keepalive_user_stream(&self, listen_key: &str) -> Result<()> {
        let _: serde_json::Value = self
            .send_request(
                Method::PUT,
                "/fapi/v1/listenKey",
                vec![("listenKey", listen_key.to_string())],
                AuthMode::ApiKey,
            )
            .await?;
        Ok(())
    }

    async fn send_request<T>(
        &self,
        method: Method,
        path: &str,
        params: Vec<(&str, String)>,
        auth_mode: AuthMode,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            if attempt == 0
                && matches!(auth_mode, AuthMode::Signed)
                && self.should_refresh_time_sync_before_signed_request()
            {
                self.sync_server_time_offset().await?;
            }
            let mut request_params = params.clone();
            if matches!(auth_mode, AuthMode::Signed) {
                request_params.push(("timestamp", self.signed_timestamp_ms().to_string()));
                request_params.push(("recvWindow", DEFAULT_RECV_WINDOW_MS.to_string()));
            }

            let query = encode_query(&request_params);
            let signed_query = if matches!(auth_mode, AuthMode::Signed) {
                format!("{query}&signature={}", self.sign_query(&query))
            } else {
                query
            };
            let url = if signed_query.is_empty() {
                format!("{}{}", self.base_url, path)
            } else {
                format!("{}{}?{}", self.base_url, path, signed_query)
            };

            let mut request = self.http.request(method.clone(), &url);
            if matches!(auth_mode, AuthMode::ApiKey | AuthMode::Signed) {
                request = request.header("X-MBX-APIKEY", &self.api_key);
            }

            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    let status = response.status();
                    let body = response
                        .text()
                        .await
                        .with_context(|| format!("failed to read response body for {path}"))?;
                    return serde_json::from_str::<T>(&body).with_context(|| {
                        format!(
                            "failed to deserialize response for {path} with status {status}: {}",
                            body_preview(&body)
                        )
                    });
                }
                Ok(response) => {
                    let status = response.status();
                    let retry_after = retry_after_delay(response.headers());
                    let body = response.text().await.unwrap_or_default();

                    if matches!(auth_mode, AuthMode::Signed)
                        && is_timestamp_out_of_window(status, &body)
                        && attempt + 1 < MAX_RETRIES
                    {
                        self.sync_server_time_offset().await?;
                        continue;
                    }

                    let error = anyhow!(
                        "request {} {} failed with status {}: {}",
                        method,
                        path,
                        status,
                        body
                    );

                    if !is_retryable_status(status) || attempt + 1 == MAX_RETRIES {
                        return Err(error);
                    }

                    last_error = Some(error);
                    sleep(retry_delay(retry_after, attempt)).await;
                    continue;
                }
                Err(error) => {
                    if attempt + 1 == MAX_RETRIES {
                        return Err(error)
                            .with_context(|| format!("request {} {} failed", method, path));
                    }
                    last_error = Some(error.into());
                }
            }

            sleep(retry_delay(None, attempt)).await;
        }

        Err(last_error.unwrap_or_else(|| anyhow!("request {} {} failed", method, path)))
    }

    fn signed_timestamp_ms(&self) -> i64 {
        (self.timestamp_provider)() + self.timestamp_offset_ms.load(Ordering::Relaxed)
    }

    async fn sync_server_time_offset(&self) -> Result<chrono::DateTime<Utc>> {
        let requested_at = (self.timestamp_provider)();
        let response = self.send_server_time_request().await?;
        let received_at = (self.timestamp_provider)();
        let midpoint = requested_at + ((received_at - requested_at) / 2);
        self.timestamp_offset_ms
            .store(response.server_time - midpoint, Ordering::Relaxed);
        self.last_time_sync_at_ms
            .store(received_at, Ordering::Relaxed);

        Utc.timestamp_millis_opt(response.server_time)
            .single()
            .context("invalid server time timestamp")
    }

    fn should_refresh_time_sync_before_signed_request(&self) -> bool {
        let last_time_sync_at_ms = self.last_time_sync_at_ms.load(Ordering::Relaxed);
        last_time_sync_at_ms > 0
            && (self.timestamp_provider)() - last_time_sync_at_ms >= SIGNED_TIME_SYNC_REFRESH_INTERVAL_MS
    }

    async fn send_server_time_request(&self) -> Result<ServerTimeResponse> {
        let mut last_error = None;
        let url = format!("{}{}", self.base_url, "/fapi/v1/time");

        for attempt in 0..MAX_RETRIES {
            match self.http.request(Method::GET, &url).send().await {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json::<ServerTimeResponse>()
                        .await
                        .context("failed to deserialize response for /fapi/v1/time");
                }
                Ok(response) => {
                    let status = response.status();
                    let retry_after = retry_after_delay(response.headers());
                    let body = response.text().await.unwrap_or_default();
                    let error = anyhow!(
                        "request GET /fapi/v1/time failed with status {}: {}",
                        status,
                        body
                    );

                    if !is_retryable_status(status) || attempt + 1 == MAX_RETRIES {
                        return Err(error);
                    }

                    last_error = Some(error);
                    sleep(retry_delay(retry_after, attempt)).await;
                }
                Err(error) => {
                    if attempt + 1 == MAX_RETRIES {
                        return Err(error).context("request GET /fapi/v1/time failed");
                    }
                    last_error = Some(error.into());
                    sleep(retry_delay(None, attempt)).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("request GET /fapi/v1/time failed")))
    }
}

#[derive(serde::Deserialize)]
struct ServerTimeResponse {
    #[serde(rename = "serverTime")]
    server_time: i64,
}

#[derive(serde::Deserialize)]
struct BinanceErrorResponse {
    code: i64,
}

fn encode_query(params: &[(&str, String)]) -> String {
    let mut serializer = Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn build_http_client(base_url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15));
    if should_bypass_proxy(base_url) {
        builder = builder.no_proxy();
    }

    builder.build().expect("failed to build reqwest client")
}

fn should_bypass_proxy(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };

    match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(host)) => IpAddr::V4(host).is_loopback(),
        Some(Host::Ipv6(host)) => IpAddr::V6(host).is_loopback(),
        None => false,
    }
}

fn format_decimal(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }

    for scale in 0..=MAX_DECIMAL_SCALE {
        let factor = 10_f64.powi(scale as i32);
        let scaled = value * factor;
        let rounded = scaled.round();
        let tolerance = scaled.abs().max(1.0) * f64::EPSILON * 16.0;
        if (scaled - rounded).abs() <= tolerance {
            let normalized = rounded / factor;
            return trim_decimal_string(format!("{normalized:.scale$}", scale = scale as usize));
        }
    }

    value.to_string()
}

fn trim_decimal_string(mut value: String) -> String {
    if value.contains('.') {
        while value.ends_with('0') {
            value.pop();
        }
        if value.ends_with('.') {
            value.pop();
        }
    }

    if value == "-0" {
        "0".to_string()
    } else {
        value
    }
}

fn side_to_binance(side: poise_core::types::Side) -> &'static str {
    match side {
        poise_core::types::Side::Buy => "BUY",
        poise_core::types::Side::Sell => "SELL",
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 418
}

fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn retry_delay(retry_after: Option<Duration>, attempt: usize) -> Duration {
    retry_after.unwrap_or_else(|| Duration::from_millis(50 * (1_u64 << attempt)))
}

fn body_preview(body: &str) -> String {
    const BODY_PREVIEW_LIMIT: usize = 256;
    if body.len() <= BODY_PREVIEW_LIMIT {
        format!("response body `{body}`")
    } else {
        format!(
            "response body `{}...`",
            &body[..BODY_PREVIEW_LIMIT]
        )
    }
}

fn is_timestamp_out_of_window(status: StatusCode, body: &str) -> bool {
    status == StatusCode::BAD_REQUEST
        && serde_json::from_str::<BinanceErrorResponse>(body)
            .map(|error| error.code == -1021)
            .unwrap_or_else(|_| body.contains(r#""code":-1021"#))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        collections::VecDeque,
        sync::{
            Arc,
            atomic::{AtomicI64, Ordering},
        },
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    use super::*;

    #[test]
    fn signs_query_with_hmac_sha256() {
        let client = BinanceRestClient::with_timestamp_provider(
            "http://127.0.0.1:0",
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
        );

        let signature =
            client.sign_query("symbol=BTCUSDT&timestamp=1700000000000&recvWindow=10000");

        assert_eq!(
            signature,
            "1b0ee80735fdeea45d65fdf993862fcae44ad51d623a7c00bc0f5ceaa3052a05"
        );
    }

    #[test]
    fn format_decimal_trims_binary_float_noise() {
        assert_eq!(format_decimal(0.1 + 0.2), "0.3");
        assert_eq!(format_decimal(0.024000000000000004), "0.024");
        assert_eq!(format_decimal(65_853.7 + 0.1), "65853.8");
        assert_eq!(format_decimal(1.23456789), "1.23456789");
    }

    #[tokio::test]
    async fn retries_open_orders_request_until_success() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(500, r#"{"code":-1000,"msg":"temporary"}"#),
            MockResponse::json(500, r#"{"code":-1000,"msg":"temporary"}"#),
            MockResponse::json(
                200,
                r#"[{
                    "symbol": "BTCUSDT",
                    "orderId": 1001,
                    "clientOrderId": "grid-open-003",
                    "side": "BUY",
                    "price": "64000.1",
                    "origQty": "0.015",
                    "status": "NEW"
                }]"#,
            ),
        ])
        .await;
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
        );

        let orders = client.get_open_orders("BTCUSDT").await.unwrap();
        let requests = server.requests().await;

        assert_eq!(orders.len(), 1);
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0].path,
            "/fapi/v1/openOrders?symbol=BTCUSDT&timestamp=1700000000000&recvWindow=10000&signature=1b0ee80735fdeea45d65fdf993862fcae44ad51d623a7c00bc0f5ceaa3052a05"
        );
        assert_eq!(
            requests[0].headers.get("x-mbx-apikey"),
            Some(&"api-key".to_string())
        );
    }

    #[tokio::test]
    async fn retries_rate_limited_request_using_retry_after() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(429, r#"{"code":-1003,"msg":"too many requests"}"#)
                .with_header("retry-after", "0"),
            MockResponse::json(
                200,
                r#"[{
                    "symbol": "BTCUSDT",
                    "orderId": 1002,
                    "clientOrderId": "grid-open-004",
                    "side": "SELL",
                    "price": "64010.1",
                    "origQty": "0.025",
                    "status": "NEW"
                }]"#,
            ),
        ])
        .await;
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
        );

        let orders = client.get_open_orders("BTCUSDT").await.unwrap();
        let requests = server.requests().await;

        assert_eq!(orders.len(), 1);
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn re_signs_signed_request_for_each_retry_attempt() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(429, r#"{"code":-1003,"msg":"too many requests"}"#)
                .with_header("retry-after", "0"),
            MockResponse::json(
                200,
                r#"[{
                    "symbol": "BTCUSDT",
                    "orderId": 1003,
                    "clientOrderId": "grid-open-005",
                    "side": "BUY",
                    "price": "64020.1",
                    "origQty": "0.035",
                    "status": "NEW"
                }]"#,
            ),
        ])
        .await;
        let next_timestamp = Arc::new(AtomicI64::new(1_700_000_000_000));
        let timestamp_provider = {
            let next_timestamp = Arc::clone(&next_timestamp);
            Arc::new(move || next_timestamp.fetch_add(6_000, Ordering::SeqCst))
        };
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            timestamp_provider,
        );

        let _ = client.get_open_orders("BTCUSDT").await.unwrap();
        let requests = server.requests().await;

        assert_eq!(requests.len(), 2);
        assert!(requests[0].path.contains("timestamp=1700000000000"));
        assert!(requests[1].path.contains("timestamp=1700000006000"));
        assert_ne!(requests[0].path, requests[1].path);
    }

    #[tokio::test]
    async fn get_server_time_calibrates_future_signed_requests() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, r#"{"serverTime":1700000005000}"#),
            MockResponse::json(
                200,
                r#"[{
                    "symbol": "BTCUSDT",
                    "orderId": 1005,
                    "clientOrderId": "grid-open-007",
                    "side": "BUY",
                    "price": "64020.1",
                    "origQty": "0.035",
                    "status": "NEW"
                }]"#,
            ),
        ])
        .await;
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
        );

        let server_time = client.get_server_time().await.unwrap();
        let _ = client.get_open_orders("BTCUSDT").await.unwrap();
        let requests = server.requests().await;

        assert_eq!(
            server_time,
            Utc.timestamp_millis_opt(1_700_000_005_000).unwrap()
        );
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].path, "/fapi/v1/time");
        assert!(requests[1].path.contains("timestamp=1700000005000"));
    }

    #[tokio::test]
    async fn retries_after_syncing_server_time_when_timestamp_is_outside_recv_window() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                400,
                r#"{"code":-1021,"msg":"Timestamp for this request is outside of the recvWindow."}"#,
            ),
            MockResponse::json(200, r#"{"serverTime":1700000005000}"#),
            MockResponse::json(
                200,
                r#"[{
                    "symbol": "BTCUSDT",
                    "orderId": 1006,
                    "clientOrderId": "grid-open-008",
                    "side": "SELL",
                    "price": "64030.1",
                    "origQty": "0.045",
                    "status": "NEW"
                }]"#,
            ),
        ])
        .await;
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
        );

        let orders = client.get_open_orders("BTCUSDT").await.unwrap();
        let requests = server.requests().await;

        assert_eq!(orders.len(), 1);
        assert_eq!(requests.len(), 3);
        assert!(requests[0].path.contains("timestamp=1700000000000"));
        assert_eq!(requests[1].path, "/fapi/v1/time");
        assert!(requests[2].path.contains("timestamp=1700000005000"));
    }

    #[tokio::test]
    async fn refreshes_server_time_before_signed_request_when_prior_sync_is_stale() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, r#"{"serverTime":1700000005000}"#),
            MockResponse::json(200, r#"{"serverTime":1700000025000}"#),
            MockResponse::json(
                200,
                r#"[{
                    "symbol": "BTCUSDT",
                    "orderId": 1007,
                    "clientOrderId": "grid-open-009",
                    "side": "SELL",
                    "price": "64030.1",
                    "origQty": "0.045",
                    "status": "NEW"
                }]"#,
            ),
        ])
        .await;
        let next_timestamp = Arc::new(AtomicI64::new(1_700_000_000_000));
        let timestamp_provider = {
            let next_timestamp = Arc::clone(&next_timestamp);
            Arc::new(move || next_timestamp.load(Ordering::SeqCst))
        };
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            timestamp_provider,
        );

        let _ = client.get_server_time().await.unwrap();
        next_timestamp.store(1_700_000_070_000, Ordering::SeqCst);

        let orders = client.get_open_orders("BTCUSDT").await.unwrap();
        let requests = server.requests().await;

        assert_eq!(orders.len(), 1);
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].path, "/fapi/v1/time");
        assert_eq!(requests[1].path, "/fapi/v1/time");
        assert!(requests[2].path.contains("timestamp=1700000025000"));
    }

    #[tokio::test]
    async fn new_order_serializes_price_and_quantity_without_float_noise() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{
                "orderId": 1004,
                "clientOrderId": "grid-open-006",
                "status": "NEW"
            }"#,
        )])
        .await;
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
        );
        let request = OrderRequest {
            instrument: poise_engine::track::Instrument::new(
                poise_engine::track::Venue::Binance,
                "BTCUSDT",
            ),
            side: poise_core::types::Side::Buy,
            price: 0.1 + 0.2,
            quantity: 0.024000000000000004,
            client_order_id: "grid-open-006".to_string(),
            reduce_only: false,
        };

        let _ = client.new_order(&request).await.unwrap();
        let requests = server.requests().await;

        assert_eq!(requests.len(), 1);
        assert!(requests[0].path.contains("price=0.3"));
        assert!(requests[0].path.contains("quantity=0.024"));
        assert!(!requests[0].path.contains("price=0.30000000000000004"));
        assert!(!requests[0].path.contains("quantity=0.024000000000000004"));
    }

    #[tokio::test]
    async fn account_margin_snapshot_reads_account_endpoint_and_maps_capacity() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{
                "availableBalance": "100.5",
                "totalWalletBalance": "120.75",
                "positions": [
                    { "symbol": "ETHUSDT", "leverage": "5" },
                    { "symbol": "BTCUSDT", "leverage": "20" }
                ]
            }"#,
        )])
        .await;
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
        );

        let snapshot = client.get_account_margin_snapshot("BTCUSDT").await.unwrap();
        let requests = server.requests().await;

        assert_eq!(snapshot.available_balance, 100.5);
        assert_eq!(snapshot.total_wallet_balance, 120.75);
        assert!((snapshot.max_increase_notional - 2010.0).abs() < f64::EPSILON);
        assert_eq!(requests.len(), 1);
        assert!(requests[0].path.starts_with("/fapi/v2/account?timestamp="));
        assert_eq!(
            requests[0].headers.get("x-mbx-apikey"),
            Some(&"api-key".to_string())
        );
    }

    #[tokio::test]
    async fn request_times_out_when_server_does_not_respond() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        std::mem::forget(listener);

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(50))
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let client = BinanceRestClient::with_http_client_and_timestamp_provider(
            format!("http://{}", address),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
            http,
        );

        let result = tokio::time::timeout(Duration::from_secs(2), client.get_server_time()).await;

        assert!(
            result.is_ok(),
            "request should complete without external timeout"
        );
        assert!(result.unwrap().is_err(), "request should fail with timeout");
    }

    #[tokio::test]
    async fn deserialize_failure_includes_response_body_preview() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"unexpected":"payload","detail":"not-an-open-order-array"}"#,
        )])
        .await;
        let client = BinanceRestClient::with_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
        );

        let error = client.get_open_orders("BTCUSDT").await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("failed to deserialize response for /fapi/v1/openOrders"));
        assert!(message.contains("unexpected"));
        assert!(message.contains("not-an-open-order-array"));
    }

    #[derive(Debug, Clone)]
    struct MockResponse {
        status: u16,
        body: String,
        headers: Vec<(String, String)>,
    }

    impl MockResponse {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
                headers: Vec::new(),
            }
        }

        fn with_header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_string(), value.to_string()));
            self
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
    }

    struct MockHttpServer {
        base_url: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl MockHttpServer {
        async fn spawn(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let queued_responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let stored_requests = Arc::clone(&requests);

            tokio::spawn(async move {
                loop {
                    let response = {
                        let mut queue = queued_responses.lock().await;
                        queue.pop_front()
                    };

                    let Some(response) = response else {
                        break;
                    };

                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 1024];

                    loop {
                        let read = stream.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }

                    let request_text = String::from_utf8(buffer).unwrap();
                    let mut lines = request_text.split("\r\n");
                    let request_line = lines.next().unwrap();
                    let mut request_line_parts = request_line.split_whitespace();
                    let method = request_line_parts.next().unwrap().to_string();
                    let path = request_line_parts.next().unwrap().to_string();
                    let mut headers = HashMap::new();

                    for line in lines.by_ref() {
                        if line.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            headers
                                .insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                        }
                    }

                    stored_requests.lock().await.push(RecordedRequest {
                        method,
                        path,
                        headers,
                    });

                    let extra_headers = response
                        .headers
                        .iter()
                        .map(|(name, value)| format!("{name}: {value}\r\n"))
                        .collect::<String>();
                    let reply = format!(
                        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{}\
\r\n{}",
                        response.status,
                        reason_phrase(response.status),
                        response.body.len(),
                        extra_headers,
                        response.body
                    );

                    stream.write_all(reply.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
            });

            Self {
                base_url: format!("http://{}", address),
                requests,
            }
        }

        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        async fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().await.clone()
        }
    }

    fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Unknown",
        }
    }
}
