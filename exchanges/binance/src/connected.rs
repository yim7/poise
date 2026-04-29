use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use poise_core::track::Instrument;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, AccountSummaryPort, AccountSummarySnapshot, ExchangeInfo,
    ExchangeOpenOrderSnapshot, ExecutionPort, ExecutionPortError, MarketDataPort, MarketDataTick,
    MetadataPort, OrderReceipt, OrderRequest, Position, UserDataEvent,
};

use crate::{
    Config,
    rest::{BinanceRestClient, BinanceRestError},
    ws::BinanceWsClient,
};

pub async fn connect(config: &Config) -> Result<Connected> {
    let endpoints = config.endpoints();
    let (api_key, api_secret) = config.credentials()?;
    let rest = Arc::new(BinanceRestClient::new(
        endpoints.rest_base_url(),
        api_key,
        api_secret,
    ));
    let ws = Arc::new(BinanceWsClient::with_ws_routes(
        Arc::clone(&rest),
        endpoints.public_ws_base_url(),
        endpoints.market_ws_base_url(),
        endpoints.user_ws_base_url(),
    ));

    Ok(Connected::from_clients(rest, ws))
}

#[derive(Clone)]
pub struct Connected {
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
}

impl Connected {
    fn from_clients(rest: Arc<BinanceRestClient>, ws: Arc<BinanceWsClient>) -> Self {
        Self::from_parts(
            Arc::new(BinanceExecution::new(Arc::clone(&rest))),
            Arc::new(BinanceMarketData::new(Arc::clone(&ws))),
            Arc::new(BinanceAccountSummary::new(Arc::clone(&rest))),
            Arc::new(BinanceAccount::new(Arc::clone(&rest), ws)),
            Arc::new(BinanceMetadata::new(rest)),
        )
    }

    fn from_parts(
        execution: Arc<dyn ExecutionPort>,
        market_data: Arc<dyn MarketDataPort>,
        account_summary: Arc<dyn AccountSummaryPort>,
        account: Arc<dyn AccountPort>,
        metadata: Arc<dyn MetadataPort>,
    ) -> Self {
        Self {
            execution,
            market_data,
            account_summary,
            account,
            metadata,
        }
    }

    pub fn execution(&self) -> Arc<dyn ExecutionPort> {
        Arc::clone(&self.execution)
    }

    pub fn market_data(&self) -> Arc<dyn MarketDataPort> {
        Arc::clone(&self.market_data)
    }

    pub fn account_summary(&self) -> Arc<dyn AccountSummaryPort> {
        Arc::clone(&self.account_summary)
    }

    pub fn account(&self) -> Arc<dyn AccountPort> {
        Arc::clone(&self.account)
    }

    pub fn metadata(&self) -> Arc<dyn MetadataPort> {
        Arc::clone(&self.metadata)
    }
}

struct BinanceExecution {
    rest: Arc<BinanceRestClient>,
}

impl BinanceExecution {
    fn new(rest: Arc<BinanceRestClient>) -> Self {
        Self { rest }
    }
}

struct BinanceMarketData {
    ws: Arc<BinanceWsClient>,
}

impl BinanceMarketData {
    fn new(ws: Arc<BinanceWsClient>) -> Self {
        Self { ws }
    }
}

struct BinanceAccountSummary {
    rest: Arc<BinanceRestClient>,
}

impl BinanceAccountSummary {
    fn new(rest: Arc<BinanceRestClient>) -> Self {
        Self { rest }
    }
}

struct BinanceAccount {
    rest: Arc<BinanceRestClient>,
    ws: Arc<BinanceWsClient>,
}

impl BinanceAccount {
    fn new(rest: Arc<BinanceRestClient>, ws: Arc<BinanceWsClient>) -> Self {
        Self { rest, ws }
    }
}

struct BinanceMetadata {
    rest: Arc<BinanceRestClient>,
}

impl BinanceMetadata {
    fn new(rest: Arc<BinanceRestClient>) -> Self {
        Self { rest }
    }
}

#[async_trait]
impl AccountSummaryPort for BinanceAccountSummary {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        self.rest.get_account_summary().await
    }
}

#[async_trait]
impl ExecutionPort for BinanceExecution {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        self.rest.new_order(&req).await
    }

    async fn cancel_order(&self, instrument: &Instrument, order_id: &str) -> Result<OrderReceipt> {
        match self.rest.cancel_order(&instrument.symbol, order_id).await {
            Ok(receipt) => Ok(receipt),
            Err(error)
                if error
                    .downcast_ref::<BinanceRestError>()
                    .is_some_and(BinanceRestError::is_cancel_outcome_unknown) =>
            {
                Err(ExecutionPortError::cancel_outcome_unknown(error.to_string()).into())
            }
            Err(error) => Err(error),
        }
    }

    async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
        self.rest.cancel_all_orders(&instrument.symbol).await?;
        Ok(())
    }

    async fn get_position(&self, instrument: &Instrument) -> Result<Position> {
        self.rest.get_position(&instrument.symbol).await
    }

    async fn get_open_orders(&self, instrument: &Instrument) -> Result<ExchangeOpenOrderSnapshot> {
        self.rest
            .get_open_orders(&instrument.symbol)
            .await
            .map(ExchangeOpenOrderSnapshot::from_complete_exchange_query)
    }
}

#[async_trait]
impl AccountPort for BinanceAccount {
    async fn get_account_capacity_snapshot(
        &self,
        instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot> {
        self.rest
            .get_account_capacity_snapshot(&instrument.symbol)
            .await
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        self.ws.subscribe_user_data().await
    }
}

#[async_trait]
impl MetadataPort for BinanceMetadata {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
        self.rest.get_exchange_info(&instrument.symbol).await
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
        self.rest.get_server_time().await
    }
}

#[async_trait]
impl MarketDataPort for BinanceMarketData {
    async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        self.ws.subscribe_prices(instrument).await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, collections::VecDeque, sync::Arc};

    use futures_util::SinkExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
        time::timeout,
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use poise_core::{
        track::{Instrument, Venue},
        types::Side,
    };
    use poise_engine::ports::{ExecutionPortError, ExecutionPortErrorKind};

    use super::*;

    struct FakeExecutionPort;
    struct FakeMarketDataPort;
    struct FakeAccountSummaryPort;
    struct FakeAccountPort;
    struct FakeMetadataPort;

    #[async_trait]
    impl ExecutionPort for FakeExecutionPort {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            unreachable!("not used in test")
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> Result<OrderReceipt> {
            unreachable!("not used in test")
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            unreachable!("not used in test")
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            unreachable!("not used in test")
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> Result<ExchangeOpenOrderSnapshot> {
            unreachable!("not used in test")
        }
    }

    #[async_trait]
    impl MarketDataPort for FakeMarketDataPort {
        async fn subscribe_prices(
            &self,
            _instrument: &Instrument,
        ) -> Result<mpsc::Receiver<MarketDataTick>> {
            unreachable!("not used in test")
        }
    }

    #[async_trait]
    impl AccountSummaryPort for FakeAccountSummaryPort {
        async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
            unreachable!("not used in test")
        }
    }

    #[async_trait]
    impl AccountPort for FakeAccountPort {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> Result<AccountCapacitySnapshot> {
            unreachable!("not used in test")
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            unreachable!("not used in test")
        }
    }

    #[async_trait]
    impl MetadataPort for FakeMetadataPort {
        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            unreachable!("not used in test")
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
            unreachable!("not used in test")
        }
    }

    fn build_test_connected() -> Connected {
        build_test_connected_with_urls("http://127.0.0.1:18080", "ws://127.0.0.1:19080")
    }

    fn build_test_connected_with_urls(
        rest_base_url: impl Into<String>,
        ws_base_url: impl Into<String>,
    ) -> Connected {
        let rest = Arc::new(BinanceRestClient::new(
            rest_base_url,
            "api-key",
            "secret-key",
        ));
        let ws_base_url = ws_base_url.into();
        let ws = Arc::new(BinanceWsClient::with_ws_routes(
            Arc::clone(&rest),
            ws_base_url.clone(),
            ws_base_url.clone(),
            ws_base_url,
        ));
        Connected::from_clients(rest, ws)
    }

    #[test]
    fn connected_exposes_all_required_ports() {
        let connected = build_test_connected();

        let _execution: Arc<dyn ExecutionPort> = connected.execution();
        let _market_data: Arc<dyn MarketDataPort> = connected.market_data();
        let _account_summary: Arc<dyn AccountSummaryPort> = connected.account_summary();
        let _account: Arc<dyn AccountPort> = connected.account();
        let _metadata: Arc<dyn MetadataPort> = connected.metadata();
    }

    #[test]
    fn connected_can_be_built_from_distinct_port_components() {
        let connected = Connected::from_parts(
            Arc::new(FakeExecutionPort),
            Arc::new(FakeMarketDataPort),
            Arc::new(FakeAccountSummaryPort),
            Arc::new(FakeAccountPort),
            Arc::new(FakeMetadataPort),
        );

        let _execution: Arc<dyn ExecutionPort> = connected.execution();
        let _market_data: Arc<dyn MarketDataPort> = connected.market_data();
        let _account_summary: Arc<dyn AccountSummaryPort> = connected.account_summary();
        let _account: Arc<dyn AccountPort> = connected.account();
        let _metadata: Arc<dyn MetadataPort> = connected.metadata();
    }

    #[tokio::test]
    async fn submit_order_calls_rest_and_returns_receipt() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{
                "orderId": 20072994037,
                "clientOrderId": "grid-order-005",
                "status": "NEW"
            }"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let receipt = execution
            .submit_order(OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: Side::Buy,
                price: 64000.5,
                quantity: 0.01,
                client_order_id: "grid-order-005".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();
        let requests = server.requests().await;

        assert_eq!(receipt.order_id, "20072994037");
        assert_eq!(requests[0].method, "POST");
        assert!(
            requests[0]
                .path
                .starts_with("/fapi/v1/order?symbol=BTCUSDT")
        );
    }

    #[tokio::test]
    async fn submit_reduce_only_order_includes_reduce_only_param() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{
                "orderId": 20072994038,
                "clientOrderId": "grid-order-006",
                "status": "NEW"
            }"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        execution
            .submit_order(OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: Side::Sell,
                price: 64000.5,
                quantity: 0.01,
                client_order_id: "grid-order-006".to_string(),
                reduce_only: true,
            })
            .await
            .unwrap();
        let requests = server.requests().await;

        assert!(requests[0].path.contains("reduceOnly=true"));
    }

    #[tokio::test]
    async fn submit_non_reduce_only_order_omits_reduce_only_param() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{
                "orderId": 20072994039,
                "clientOrderId": "grid-order-007",
                "status": "NEW"
            }"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        execution
            .submit_order(OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: Side::Buy,
                price: 64000.5,
                quantity: 0.01,
                client_order_id: "grid-order-007".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();
        let requests = server.requests().await;

        assert!(!requests[0].path.contains("reduceOnly="));
    }

    #[tokio::test]
    async fn cancel_order_calls_rest_endpoint() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{
                "orderId": 12345,
                "clientOrderId": "grid-order-006",
                "status": "CANCELED"
            }"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        execution
            .cancel_order(&Instrument::new(Venue::Binance, "BTCUSDT"), "12345")
            .await
            .unwrap();
        let requests = server.requests().await;

        assert_eq!(requests[0].method, "DELETE");
        assert!(
            requests[0]
                .path
                .starts_with("/fapi/v1/order?symbol=BTCUSDT&orderId=12345")
        );
    }

    #[tokio::test]
    async fn cancel_order_maps_unknown_order_sent_to_exchange_neutral_port_error() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            400,
            r#"{"code":-2011,"msg":"Unknown order sent."}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .cancel_order(&Instrument::new(Venue::Binance, "BTCUSDT"), "12345")
            .await
            .unwrap_err();
        let port_error = error
            .downcast_ref::<ExecutionPortError>()
            .expect("cancel should map to exchange-neutral execution port error");

        assert_eq!(
            port_error.kind(),
            ExecutionPortErrorKind::CancelOutcomeUnknown
        );
        assert!(port_error.to_string().contains("Unknown order sent."));
    }

    #[tokio::test]
    async fn get_position_calls_rest_and_converts_payload() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"[{
                "symbol": "BTCUSDT",
                "positionAmt": "0.250",
                "entryPrice": "65000.5",
                "unRealizedProfit": "123.45"
            }]"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let position = execution
            .get_position(&Instrument::new(Venue::Binance, "BTCUSDT"))
            .await
            .unwrap();

        assert_eq!(position.instrument.symbol, "BTCUSDT");
        assert_eq!(position.qty, 0.25);
        assert_eq!(position.avg_price, 65000.5);
    }

    #[tokio::test]
    async fn account_capacity_snapshot_calls_rest_and_converts_payload() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{
                    "totalWalletBalance": "120.75",
                    "availableBalance": "100.5",
                    "totalMarginBalance": "125.25",
                    "totalUnrealizedProfit": "4.5",
                    "positions": []
                }"#,
            ),
            MockResponse::json(
                200,
                r#"[
                    {
                        "symbol": "BTCUSDT",
                        "marginType": "CROSSED",
                        "isAutoAddMargin": false,
                        "leverage": 20,
                        "maxNotionalValue": "1000000"
                    }
                ]"#,
            ),
        ])
        .await;
        let account =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").account();

        let snapshot = account
            .get_account_capacity_snapshot(&Instrument::new(Venue::Binance, "BTCUSDT"))
            .await
            .unwrap();
        let requests = server.requests().await;

        assert!((snapshot.max_increase_notional - 2010.0).abs() < f64::EPSILON);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[1].method, "GET");
        assert!(requests[0].path.starts_with("/fapi/v3/account?"));
        assert!(
            requests[1]
                .path
                .starts_with("/fapi/v1/symbolConfig?symbol=BTCUSDT&")
        );
    }

    #[tokio::test]
    async fn subscribe_prices_returns_stream_receiver() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            websocket
                .send(
                    Message::Text(
                        r#"{"e":"markPriceUpdate","E":1700000000000,"s":"BTCUSDT","p":"64000.10","i":"63999.90"}"#
                            .to_string()
                    ),
                )
                .await
                .unwrap();
            websocket.close(None).await.unwrap();
        });

        let market_data =
            build_test_connected_with_urls("http://127.0.0.1:1", format!("ws://{}", address))
                .market_data();

        let mut receiver = market_data
            .subscribe_prices(&Instrument::new(Venue::Binance, "BTCUSDT"))
            .await
            .unwrap();
        let tick = timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();

        match tick {
            MarketDataTick::MarkPrice(tick) => {
                assert_eq!(tick.instrument.symbol, "BTCUSDT");
                assert_eq!(tick.mark_price, 64000.10);
            }
            MarketDataTick::ExecutionQuote(_) => panic!("expected mark price tick"),
        }
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
                            headers
                                .insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
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
            _ => "Unknown",
        }
    }
}
