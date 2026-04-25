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

use poise_engine::ports::{
    AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt,
    OrderRequest, OrderStatus, Position,
};

use super::auth::sign_v5_payload;
use super::models::{
    BybitResponse, CancelAllRequestBody, CancelAllResult, CancelOrderRequestBody,
    CreateOrderRequestBody, CreateOrderResult, InstrumentInfoResult, OpenOrderListResult,
    PositionListResult, PositionSnapshot, ServerTimeResult, SetLeverageRequestBody,
    WalletBalanceResult,
};
use crate::Deployment;
use crate::mapper::{
    build_account_capacity_snapshot, build_bybit_position, should_track_bybit_order, side_to_bybit,
};

const DEFAULT_RECV_WINDOW_MS: i64 = 5_000;
const ACTIVE_ORDER_FILTER: &str = "Order";
const RET_CODE_LEVERAGE_NOT_MODIFIED: i64 = 110_043;

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
                None,
                AuthMode::None,
            )
            .await?;
        response.try_into()
    }

    pub async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        let body = CreateOrderRequestBody {
            category: "linear",
            symbol: req.instrument.symbol.clone(),
            side: side_to_bybit(req.side).to_string(),
            order_type: "Limit",
            qty: req.quantity.to_string(),
            price: req.price.to_string(),
            time_in_force: "GTC",
            position_idx: 0,
            order_link_id: req.client_order_id.clone(),
            reduce_only: req.reduce_only,
        };
        let response: CreateOrderResult = self
            .send_request(
                Method::POST,
                "/v5/order/create",
                Vec::new(),
                Some(
                    serde_json::to_string(&body)
                        .context("failed to serialize create order body")?,
                ),
                AuthMode::Signed,
            )
            .await?;
        response.try_into()
    }

    pub async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<OrderReceipt> {
        let body = CancelOrderRequestBody {
            category: "linear",
            symbol: symbol.to_string(),
            order_id: Some(order_id.to_string()),
            order_link_id: None,
        };
        let response: CreateOrderResult = self
            .send_request(
                Method::POST,
                "/v5/order/cancel",
                Vec::new(),
                Some(
                    serde_json::to_string(&body)
                        .context("failed to serialize cancel order body")?,
                ),
                AuthMode::Signed,
            )
            .await?;
        Ok(OrderReceipt {
            order_id: response.order_id,
            client_order_id: response.order_link_id.unwrap_or_default(),
            filled_qty: 0.0,
            status: OrderStatus::Canceled,
        })
    }

    pub async fn cancel_all(&self, symbol: &str) -> Result<()> {
        let body = CancelAllRequestBody {
            category: "linear",
            symbol: symbol.to_string(),
            order_filter: ACTIVE_ORDER_FILTER,
        };
        let response: CancelAllResult = self
            .send_request(
                Method::POST,
                "/v5/order/cancel-all",
                Vec::new(),
                Some(serde_json::to_string(&body).context("failed to serialize cancel-all body")?),
                AuthMode::Signed,
            )
            .await?;
        if response.success.as_deref() != Some("1") {
            return Err(anyhow!(
                "Bybit cancel-all acknowledgement did not confirm success for active orders"
            ));
        }
        Ok(())
    }

    pub async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        let response: WalletBalanceResult = self
            .send_request(
                Method::GET,
                "/v5/account/wallet-balance",
                vec![("accountType", "UNIFIED".to_string())],
                None,
                AuthMode::Signed,
            )
            .await?;
        response.into_account_summary_snapshot()
    }

    pub async fn get_account_capacity_snapshot(
        &self,
        symbol: &str,
    ) -> Result<AccountCapacitySnapshot> {
        let wallet_balance: WalletBalanceResult = self
            .send_request(
                Method::GET,
                "/v5/account/wallet-balance",
                vec![("accountType", "UNIFIED".to_string())],
                None,
                AuthMode::Signed,
            )
            .await?;
        let leverage = self
            .get_linear_position_snapshot(symbol)
            .await?
            .and_then(|position| position.leverage)
            .ok_or_else(|| {
                anyhow!(
                    "Bybit account capacity unavailable for `{symbol}`: position leverage missing (portfolio margin mode or stale position snapshot)"
                )
            })?;
        build_account_capacity_snapshot(&wallet_balance, leverage)
    }

    pub async fn get_position(&self, symbol: &str) -> Result<Position> {
        match self.get_linear_position_snapshot(symbol).await? {
            Some(position) => position.try_into(),
            None => build_bybit_position(symbol.to_string(), None, 0.0, Some(0.0), Some(0.0), 0),
        }
    }

    pub async fn get_open_orders(&self, symbol: &str) -> Result<Vec<ExchangeOrder>> {
        let response: OpenOrderListResult = self
            .send_request(
                Method::GET,
                "/v5/order/realtime",
                vec![
                    ("category", "linear".to_string()),
                    ("symbol", symbol.to_string()),
                    ("orderFilter", ACTIVE_ORDER_FILTER.to_string()),
                ],
                None,
                AuthMode::Signed,
            )
            .await?;
        response
            .list
            .into_iter()
            .filter_map(|order| {
                if should_track_bybit_order(order.order_status, order.stop_order_type.as_deref()) {
                    Some(order.try_into())
                } else {
                    None
                }
            })
            .collect()
    }

    pub async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        let body = SetLeverageRequestBody {
            category: "linear",
            symbol: symbol.to_string(),
            buy_leverage: leverage.to_string(),
            sell_leverage: leverage.to_string(),
        };
        let envelope: BybitResponse<serde_json::Value> = self
            .send_request_envelope(
                Method::POST,
                "/v5/position/set-leverage",
                Vec::new(),
                Some(serde_json::to_string(&body).context("failed to serialize leverage body")?),
                AuthMode::Signed,
            )
            .await?;
        if envelope.ret_code != 0 && envelope.ret_code != RET_CODE_LEVERAGE_NOT_MODIFIED {
            return Err(anyhow!(
                "request POST /v5/position/set-leverage failed with retCode {}: {}",
                envelope.ret_code,
                envelope.ret_msg.unwrap_or_default()
            ));
        }
        Ok(())
    }

    pub async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        let response: ServerTimeResult = self
            .send_request(
                Method::GET,
                "/v5/market/time",
                Vec::new(),
                None,
                AuthMode::None,
            )
            .await?;
        response.try_into()
    }

    async fn get_linear_position_snapshot(&self, symbol: &str) -> Result<Option<PositionSnapshot>> {
        let response: PositionListResult = self
            .send_request(
                Method::GET,
                "/v5/position/list",
                vec![
                    ("category", "linear".to_string()),
                    ("symbol", symbol.to_string()),
                ],
                None,
                AuthMode::Signed,
            )
            .await?;

        Ok(response.list.into_iter().next())
    }

    async fn send_request<T>(
        &self,
        method: Method,
        path: &str,
        params: Vec<(&str, String)>,
        body: Option<String>,
        auth_mode: AuthMode,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let envelope = self
            .send_request_envelope(method.clone(), path, params, body, auth_mode)
            .await?;
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

    async fn send_request_envelope<T>(
        &self,
        method: Method,
        path: &str,
        params: Vec<(&str, String)>,
        body: Option<String>,
        auth_mode: AuthMode,
    ) -> Result<BybitResponse<T>>
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
        if let Some(body) = body.as_ref() {
            request = request
                .body(body.clone())
                .header("Content-Type", "application/json");
        }
        if matches!(auth_mode, AuthMode::Signed) {
            let timestamp = self.signed_timestamp_ms();
            let signing_payload = body.as_deref().unwrap_or(&query_string);
            let sign = sign_v5_payload(
                &self.api_secret,
                timestamp,
                &self.api_key,
                self.recv_window_ms,
                signing_payload,
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

        serde_json::from_str(&body).with_context(|| {
            format!("failed to deserialize response for {path} with status {status}: {body}")
        })
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
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[]}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[]}}"#,
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
        let _ = client.get_position("BTCUSDT").await.unwrap();
        let _ = client.get_open_orders("BTCUSDT").await.unwrap();
        let requests = server.requests();

        assert_eq!(requests.len(), 4);
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
        assert_eq!(requests[2].method, "GET");
        assert_eq!(
            requests[2].path,
            "/v5/position/list?category=linear&symbol=BTCUSDT"
        );
        assert_eq!(requests[3].method, "GET");
        assert_eq!(
            requests[3].path,
            "/v5/order/realtime?category=linear&symbol=BTCUSDT&orderFilter=Order"
        );
        assert_eq!(
            requests[1].headers.get("x-bapi-api-key"),
            Some(&"api-key".to_string())
        );
        assert_eq!(
            requests[1].headers.get("x-bapi-sign"),
            Some(&sign_v5_payload(
                "secret-key",
                1_700_000_000_000,
                "api-key",
                5_000,
                "accountType=UNIFIED"
            ))
        );
    }

    #[tokio::test]
    async fn submit_order_uses_linear_limit_gtc_body_fields() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"retCode":0,"retMsg":"OK","result":{"orderId":"12345","orderLinkId":"client-1"}}"#,
        )])
        .await;
        let client = BybitRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
            build_http_client(&server.base_url()),
        );

        let receipt = client
            .submit_order(OrderRequest {
                instrument: poise_engine::track::Instrument::new(
                    poise_engine::track::Venue::Bybit,
                    "BTCUSDT",
                ),
                side: poise_core::types::Side::Buy,
                price: 64000.10,
                quantity: 0.01,
                client_order_id: "client-1".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();

        assert_eq!(receipt.order_id, "12345");
        assert_eq!(receipt.client_order_id, "client-1");
        assert_eq!(receipt.status, poise_engine::ports::OrderStatus::Submitting);

        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v5/order/create");
        assert!(request.body.contains(r#""category":"linear""#));
        assert!(request.body.contains(r#""orderType":"Limit""#));
        assert!(request.body.contains(r#""timeInForce":"GTC""#));
        assert!(request.body.contains(r#""positionIdx":0"#));
    }

    #[tokio::test]
    async fn set_leverage_uses_linear_position_body() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"retCode":0,"retMsg":"OK","result":{},"retExtInfo":{},"time":1672281607343}"#,
        )])
        .await;
        let client = BybitRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
            build_http_client(&server.base_url()),
        );

        client.set_leverage("BTCUSDT", 10).await.unwrap();

        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v5/position/set-leverage");
        assert!(request.body.contains(r#""category":"linear""#));
        assert!(request.body.contains(r#""symbol":"BTCUSDT""#));
        assert!(request.body.contains(r#""buyLeverage":"10""#));
        assert!(request.body.contains(r#""sellLeverage":"10""#));
    }

    #[tokio::test]
    async fn set_leverage_treats_not_modified_as_success() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"retCode":110043,"retMsg":"leverage not modified","result":{},"retExtInfo":{},"time":1672281607343}"#,
        )])
        .await;
        let client = BybitRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
            build_http_client(&server.base_url()),
        );

        client.set_leverage("BTCUSDT", 10).await.unwrap();
    }

    #[tokio::test]
    async fn account_capacity_snapshot_scales_available_balance_by_symbol_leverage() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"accountType":"UNIFIED","totalEquity":"125.5","totalAvailableBalance":"100.25","totalPerpUPL":"-2.75"}]}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"symbol":"BTCUSDT","side":"","size":"0","avgPrice":"","unrealisedPnl":"","positionIdx":0,"leverage":"10"}]}}"#,
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

        let snapshot = client
            .get_account_capacity_snapshot("BTCUSDT")
            .await
            .unwrap();

        assert_eq!(snapshot.max_increase_notional, 1002.5);
    }

    #[tokio::test]
    async fn account_capacity_snapshot_returns_clear_error_when_position_leverage_is_missing() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"accountType":"UNIFIED","totalEquity":"125.5","totalAvailableBalance":"100.25","totalPerpUPL":"-2.75"}]}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"symbol":"BTCUSDT","side":"","size":"0","avgPrice":"","unrealisedPnl":"","positionIdx":0,"leverage":""}]}}"#,
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

        let error = client
            .get_account_capacity_snapshot("BTCUSDT")
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Bybit account capacity unavailable for `BTCUSDT`: position leverage missing (portfolio margin mode or stale position snapshot)"
        );
    }

    #[tokio::test]
    async fn cancel_all_uses_symbol_body() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"retCode":0,"retMsg":"OK","result":{"list":[],"success":"1"}}"#,
        )])
        .await;
        let client = BybitRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
            build_http_client(&server.base_url()),
        );

        client.cancel_all("BTCUSDT").await.unwrap();

        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v5/order/cancel-all");
        assert!(request.body.contains(r#""category":"linear""#));
        assert!(request.body.contains(r#""symbol":"BTCUSDT""#));
        assert!(request.body.contains(r#""orderFilter":"Order""#));
    }

    #[tokio::test]
    async fn cancel_all_rejects_unsuccessful_ack() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"retCode":0,"retMsg":"OK","result":{"list":[],"success":"0"}}"#,
        )])
        .await;
        let client = BybitRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            "api-key",
            "secret-key",
            Arc::new(|| 1_700_000_000_000),
            build_http_client(&server.base_url()),
        );

        let error = client.cancel_all("BTCUSDT").await.unwrap_err().to_string();

        assert!(error.contains("did not confirm success"));
    }

    #[test]
    fn signs_v5_payload_with_timestamp_recv_window_and_body() {
        assert_eq!(
            sign_v5_payload(
                "secret-key",
                1_700_000_000_000,
                "api-key",
                5_000,
                r#"{"symbol":"BTCUSDT"}"#
            ),
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
        body: String,
    }

    fn parse_request(raw: &str) -> RecordedRequest {
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .map(|(head, body)| (head, body.to_string()))
            .unwrap_or((raw, String::new()));
        let mut lines = head.split("\r\n");
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
            body,
        }
    }
}
