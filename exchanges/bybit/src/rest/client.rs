use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use reqwest::Method;
use serde::de::DeserializeOwned;
use tokio::time::Duration;
use url::form_urlencoded::Serializer;
use url::{Host, Url};

use poise_engine::ports::{AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo};

use super::auth::sign_v5_payload;
use super::models::{BybitResponse, InstrumentInfoResult, ServerTimeResult, WalletBalanceResult};
use crate::Deployment;
use crate::mapper::build_account_capacity_snapshot;

const DEFAULT_RECV_WINDOW_MS: i64 = 5_000;

#[derive(Debug, Clone, Copy)]
enum AuthMode {
    None,
    Signed,
}

pub struct BybitRestClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    api_secret: String,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    recv_window_ms: i64,
    timestamp_offset_ms: AtomicI64,
}

impl BybitRestClient {
    pub fn new(
        deployment: Deployment,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
    ) -> Self {
        let base_url = match deployment {
            Deployment::Mainnet => "https://api.bybit.com",
            Deployment::Testnet => "https://api-testnet.bybit.com",
        }
        .to_string();
        let http = build_http_client(&base_url);
        Self {
            http,
            base_url,
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            timestamp_provider: Arc::new(|| chrono::Utc::now().timestamp_millis()),
            recv_window_ms: DEFAULT_RECV_WINDOW_MS,
            timestamp_offset_ms: AtomicI64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_http_client_and_timestamp_provider(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            timestamp_provider,
            recv_window_ms: DEFAULT_RECV_WINDOW_MS,
            timestamp_offset_ms: AtomicI64::new(0),
        }
    }

    pub async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo> {
        let response: InstrumentInfoResult = self
            .send_request(
                Method::GET,
                "/v5/market/instruments-info",
                vec![
                    ("category", "linear".to_string()),
                    ("symbol", symbol.to_string()),
                ],
                AuthMode::None,
            )
            .await?;
        response.try_into()
    }

    pub async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        let response: WalletBalanceResult = self
            .send_request(
                Method::GET,
                "/v5/account/wallet-balance",
                vec![("accountType", "UNIFIED".to_string())],
                AuthMode::Signed,
            )
            .await?;
        response.into_account_summary_snapshot()
    }

    pub async fn get_account_capacity_snapshot(
        &self,
        _symbol: &str,
    ) -> Result<AccountCapacitySnapshot> {
        let response: WalletBalanceResult = self
            .send_request(
                Method::GET,
                "/v5/account/wallet-balance",
                vec![("accountType", "UNIFIED".to_string())],
                AuthMode::Signed,
            )
            .await?;
        build_account_capacity_snapshot(&response)
    }

    pub async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        let response: ServerTimeResult = self
            .send_request(Method::GET, "/v5/market/time", Vec::new(), AuthMode::None)
            .await?;
        response.try_into()
    }

    pub fn sign_v5_payload(&self, payload: &str) -> String {
        sign_v5_payload(
            &self.api_secret,
            self.signed_timestamp_ms(),
            &self.api_key,
            self.recv_window_ms,
            payload,
        )
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
        let query_string = encode_query(&params);
        let url = if query_string.is_empty() {
            format!("{}{}", self.base_url, path)
        } else {
            format!("{}{}?{}", self.base_url, path, query_string)
        };

        let mut request = self.http.request(method.clone(), &url);
        if matches!(auth_mode, AuthMode::Signed) {
            let timestamp = self.signed_timestamp_ms();
            let sign = sign_v5_payload(
                &self.api_secret,
                timestamp,
                &self.api_key,
                self.recv_window_ms,
                &query_string,
            );
            request = request
                .header("X-BAPI-API-KEY", &self.api_key)
                .header("X-BAPI-TIMESTAMP", timestamp.to_string())
                .header("X-BAPI-RECV-WINDOW", self.recv_window_ms.to_string())
                .header("X-BAPI-SIGN", sign);
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("request {} {} failed", method, path))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read response body for {path}"))?;

        if !status.is_success() {
            return Err(anyhow!(
                "request {} {} failed with status {}: {}",
                method,
                path,
                status,
                body
            ));
        }

        let envelope: BybitResponse<T> = serde_json::from_str(&body).with_context(|| {
            format!("failed to deserialize response for {path} with status {status}: {body}")
        })?;
        if envelope.ret_code != 0 {
            return Err(anyhow!(
                "request {} {} failed with retCode {}: {}",
                method,
                path,
                envelope.ret_code,
                envelope.ret_msg.unwrap_or_default()
            ));
        }

        Ok(envelope.result)
    }

    fn signed_timestamp_ms(&self) -> i64 {
        (self.timestamp_provider)() + self.timestamp_offset_ms.load(Ordering::Relaxed)
    }
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

fn encode_query(params: &[(&str, String)]) -> String {
    let mut serializer = Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn should_bypass_proxy(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };

    match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(host)) => std::net::IpAddr::V4(host).is_loopback(),
        Some(Host::Ipv6(host)) => std::net::IpAddr::V6(host).is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    #[tokio::test]
    async fn requests_include_required_query_parameters_for_public_and_private_calls() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"symbol":"BTCUSDT","priceFilter":{"tickSize":"0.10"},"lotSizeFilter":{"qtyStep":"0.001","minOrderQty":"0.001","minNotionalValue":"5"}}]}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"accountType":"UNIFIED","totalEquity":"125.5","totalAvailableBalance":"100.25","totalPerpUPL":"-2.75"}]}}"#,
            ),
        ])
        .await;
        let client = BybitRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
            build_http_client(&server.base_url()),
        );

        let _ = client.get_exchange_info("BTCUSDT").await.unwrap();
        let _ = client.get_account_summary().await.unwrap();
        let requests = server.requests();

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].path,
            "/v5/market/instruments-info?category=linear&symbol=BTCUSDT"
        );
        assert_eq!(requests[1].method, "GET");
        assert_eq!(
            requests[1].path,
            "/v5/account/wallet-balance?accountType=UNIFIED"
        );
        assert_eq!(
            requests[1].headers.get("x-bapi-api-key"),
            Some(&"api-key".to_string())
        );
        assert_eq!(
            requests[1].headers.get("x-bapi-sign"),
            Some(&client.sign_v5_payload("accountType=UNIFIED"))
        );
    }

    #[test]
    fn signs_v5_payload_with_timestamp_recv_window_and_body() {
        let client = BybitRestClient::with_http_client_and_timestamp_provider(
            "http://127.0.0.1:0",
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
            build_http_client("http://127.0.0.1:0"),
        );

        assert_eq!(
            client.sign_v5_payload(r#"{"symbol":"BTCUSDT"}"#),
            "c12472cfb89cef80a14dcb760f2e33587a62b444a4dcfb6d243342752d34051d"
        );
    }

    #[derive(Debug, Clone)]
    struct MockResponse {
        status: u16,
        body: String,
    }

    impl MockResponse {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
            }
        }
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
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let mut buffer = vec![0_u8; 4096];
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    let request = parse_request(&String::from_utf8_lossy(&buffer[..read]));
                    stored_requests.lock().unwrap().push(request);

                    let response = queued_responses.lock().unwrap().pop_front().unwrap();
                    let status_text = if response.status == 200 { "OK" } else { "ERR" };
                    let raw = format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                        response.status,
                        status_text,
                        response.body.len(),
                        response.body
                    );
                    socket.write_all(raw.as_bytes()).await.unwrap();
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

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
    }

    fn parse_request(raw: &str) -> RecordedRequest {
        let mut lines = raw.split("\r\n");
        let request_line = lines.next().unwrap();
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap().to_string();
        let path = request_parts.next().unwrap().to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        RecordedRequest {
            method,
            path,
            headers,
        }
    }
}
