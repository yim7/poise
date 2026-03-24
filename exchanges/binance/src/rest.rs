use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use hmac::{Hmac, Mac};
use reqwest::Method;
use serde::de::DeserializeOwned;
use sha2::Sha256;
use tokio::time::{Duration, sleep};
use url::form_urlencoded::Serializer;

use grid_engine::ports::{ExchangeInfo, OpenOrder, OrderReceipt, OrderRequest, Position};

use crate::types::{
    BinanceExchangeInfoResponse, BinanceOpenOrder, BinanceOrderResponse, BinancePositionRisk,
};

const DEFAULT_RECV_WINDOW_MS: i64 = 5_000;
const MAX_RETRIES: usize = 3;

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
}

impl BinanceRestClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timestamp_provider: Arc::new(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    #[cfg(test)]
    fn with_timestamp_provider(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timestamp_provider,
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
            .send_request(Method::GET, "/fapi/v1/exchangeInfo", Vec::new(), AuthMode::None)
            .await?;

        let symbol_info = response
            .symbols
            .into_iter()
            .find(|item| item.symbol == symbol)
            .with_context(|| format!("symbol not found in exchange info: {symbol}"))?;

        symbol_info.try_into()
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

    pub async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>> {
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

    pub async fn new_order(&self, req: &OrderRequest) -> Result<OrderReceipt> {
        let response: BinanceOrderResponse = self
            .send_request(
                Method::POST,
                "/fapi/v1/order",
                vec![
                    ("symbol", req.symbol.clone()),
                    ("side", side_to_binance(req.side).to_string()),
                    ("type", "LIMIT".to_string()),
                    ("timeInForce", "GTC".to_string()),
                    ("quantity", req.quantity.to_string()),
                    ("price", req.price.to_string()),
                    ("newClientOrderId", req.client_order_id.clone()),
                ],
                AuthMode::Signed,
            )
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
            .send_request(Method::POST, "/fapi/v1/listenKey", Vec::new(), AuthMode::ApiKey)
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

    async fn send_request<T>(&self, method: Method, path: &str, mut params: Vec<(&str, String)>, auth_mode: AuthMode) -> Result<T>
    where
        T: DeserializeOwned,
    {
        if matches!(auth_mode, AuthMode::Signed) {
            params.push(("timestamp", (self.timestamp_provider)().to_string()));
            params.push(("recvWindow", DEFAULT_RECV_WINDOW_MS.to_string()));
        }

        let query = encode_query(&params);
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

        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            let mut request = self.http.request(method.clone(), &url);
            if matches!(auth_mode, AuthMode::ApiKey | AuthMode::Signed) {
                request = request.header("X-MBX-APIKEY", &self.api_key);
            }

            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json::<T>()
                        .await
                        .with_context(|| format!("failed to deserialize response for {path}"));
                }
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    let error = anyhow!("request {} {} failed with status {}: {}", method, path, status, body);

                    if !status.is_server_error() || attempt + 1 == MAX_RETRIES {
                        return Err(error);
                    }

                    last_error = Some(error);
                }
                Err(error) => {
                    if attempt + 1 == MAX_RETRIES {
                        return Err(error).with_context(|| format!("request {} {} failed", method, path));
                    }
                    last_error = Some(error.into());
                }
            }

            sleep(Duration::from_millis(50 * (1_u64 << attempt))).await;
        }

        Err(last_error.unwrap_or_else(|| anyhow!("request {} {} failed", method, path)))
    }
}

fn encode_query(params: &[(&str, String)]) -> String {
    let mut serializer = Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn side_to_binance(side: grid_core::types::Side) -> &'static str {
    match side {
        grid_core::types::Side::Buy => "BUY",
        grid_core::types::Side::Sell => "SELL",
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, collections::VecDeque, sync::Arc};

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
            client.sign_query("symbol=BTCUSDT&timestamp=1700000000000&recvWindow=5000");

        assert_eq!(
            signature,
            "8060b5a3659c282a31f2af0a1f52a97899704f79df4ef332ad3ba05390884195"
        );
    }

    #[tokio::test]
    async fn retries_open_orders_request_until_success() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(500, r#"{"code":-1000,"msg":"temporary"}"#),
            MockResponse::json(500, r#"{"code":-1000,"msg":"temporary"}"#),
            MockResponse::json(
                200,
                r#"[{
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
            "/fapi/v1/openOrders?symbol=BTCUSDT&timestamp=1700000000000&recvWindow=5000&signature=8060b5a3659c282a31f2af0a1f52a97899704f79df4ef332ad3ba05390884195"
        );
        assert_eq!(
            requests[0].headers.get("x-mbx-apikey"),
            Some(&"api-key".to_string())
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
                            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                        }
                    }

                    stored_requests.lock().await.push(RecordedRequest {
                        method,
                        path,
                        headers,
                    });

                    let reply = format!(
                        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response.status,
                        reason_phrase(response.status),
                        response.body.len(),
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
            500 => "Internal Server Error",
            _ => "Unknown",
        }
    }
}
