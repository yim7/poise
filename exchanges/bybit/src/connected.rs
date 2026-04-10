use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, AccountSummaryPort, AccountSummarySnapshot, ExchangeInfo,
    ExchangeOrder, ExecutionPort, MarketDataPort, MetadataPort, OrderReceipt, OrderRequest,
    Position, PriceTick, UserDataEvent,
};
use poise_engine::track::Instrument;

use crate::{Config, rest::BybitRestClient, ws::BybitWsClient};

pub async fn connect(config: &Config) -> Result<Connected> {
    let (api_key, api_secret) = config.credentials()?;
    let deployment = config.deployment.clone();
    let rest = Arc::new(BybitRestClient::new(
        deployment.clone(),
        api_key.clone(),
        api_secret.clone(),
    ));
    let ws = Arc::new(BybitWsClient::new(deployment, api_key, api_secret));

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
    fn from_clients(rest: Arc<BybitRestClient>, ws: Arc<BybitWsClient>) -> Self {
        Self::from_parts(
            Arc::new(BybitExecution::new(Arc::clone(&rest))),
            Arc::new(BybitMarketData::new(Arc::clone(&ws))),
            Arc::new(BybitAccountSummary::new(Arc::clone(&rest))),
            Arc::new(BybitAccount::new(Arc::clone(&rest), ws)),
            Arc::new(BybitMetadata::new(rest)),
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

struct BybitExecution {
    _rest: Arc<BybitRestClient>,
}

impl BybitExecution {
    fn new(rest: Arc<BybitRestClient>) -> Self {
        Self { _rest: rest }
    }
}

struct BybitMarketData {
    _ws: Arc<BybitWsClient>,
}

impl BybitMarketData {
    fn new(ws: Arc<BybitWsClient>) -> Self {
        Self { _ws: ws }
    }
}

struct BybitAccountSummary {
    _rest: Arc<BybitRestClient>,
}

impl BybitAccountSummary {
    fn new(rest: Arc<BybitRestClient>) -> Self {
        Self { _rest: rest }
    }
}

struct BybitAccount {
    _rest: Arc<BybitRestClient>,
    _ws: Arc<BybitWsClient>,
}

impl BybitAccount {
    fn new(rest: Arc<BybitRestClient>, ws: Arc<BybitWsClient>) -> Self {
        Self {
            _rest: rest,
            _ws: ws,
        }
    }
}

struct BybitMetadata {
    _rest: Arc<BybitRestClient>,
}

impl BybitMetadata {
    fn new(rest: Arc<BybitRestClient>) -> Self {
        Self { _rest: rest }
    }
}

#[async_trait]
impl ExecutionPort for BybitExecution {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        self._rest.submit_order(req).await
    }

    async fn cancel_order(&self, instrument: &Instrument, order_id: &str) -> Result<()> {
        self._rest.cancel_order(&instrument.symbol, order_id).await
    }

    async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
        self._rest.cancel_all(&instrument.symbol).await
    }

    async fn get_position(&self, instrument: &Instrument) -> Result<Position> {
        self._rest.get_position(&instrument.symbol).await
    }

    async fn get_open_orders(&self, instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
        self._rest.get_open_orders(&instrument.symbol).await
    }
}

#[async_trait]
impl MarketDataPort for BybitMarketData {
    async fn subscribe_prices(&self, instrument: &Instrument) -> Result<mpsc::Receiver<PriceTick>> {
        self._ws.subscribe_prices(instrument).await
    }
}

#[async_trait]
impl AccountSummaryPort for BybitAccountSummary {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        self._rest.get_account_summary().await
    }
}

#[async_trait]
impl AccountPort for BybitAccount {
    async fn get_account_capacity_snapshot(
        &self,
        instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot> {
        self._rest
            .get_account_capacity_snapshot(&instrument.symbol)
            .await
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        self._ws.subscribe_user_data().await
    }
}

#[async_trait]
impl MetadataPort for BybitMetadata {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
        self._rest.get_exchange_info(&instrument.symbol).await
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
        self._rest.get_server_time().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt, StreamExt};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{Duration, timeout},
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use poise_core::types::Side;
    use poise_engine::ports::{OrderRequest, OrderStatus};
    use poise_engine::track::{Instrument, Venue};

    use super::*;

    #[tokio::test]
    async fn connected_exposes_all_required_ports() {
        let config = Config {
            deployment: crate::Deployment::Mainnet,
            api_key: Some("demo-key".to_string()),
            api_secret: Some("demo-secret".to_string()),
        };

        let connected = connect(&config).await.unwrap();

        let _execution = connected.execution();
        let _market_data = connected.market_data();
        let _account_summary = connected.account_summary();
        let _account = connected.account();
        let _metadata = connected.metadata();
    }

    #[tokio::test]
    async fn connected_rest_ports_are_wired_to_bybit_rest_client() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"accountType":"UNIFIED","totalEquity":"125.5","totalAvailableBalance":"100.25","totalPerpUPL":"-2.75"}]}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"accountType":"UNIFIED","totalEquity":"125.5","totalAvailableBalance":"100.25","totalPerpUPL":"-2.75"}]}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"symbol":"BTCUSDT","priceFilter":{"tickSize":"0.10"},"lotSizeFilter":{"qtyStep":"0.001","minOrderQty":"0.001","minNotionalValue":"5"}}]}}"#,
            ),
            MockResponse::json(200, r#"{"retCode":0,"retMsg":"OK","result":{"timeSecond":1700000000}}"#),
        ])
        .await;

        let rest = Arc::new(
            crate::rest::BybitRestClient::with_http_client_and_timestamp_provider(
                server.base_url(),
                "api-key",
                "secret-key",
                Arc::new(|| 1_700_000_000_000),
                reqwest::Client::new(),
            ),
        );
        let ws = Arc::new(BybitWsClient::with_test_params(
            "ws://127.0.0.1:1",
            "ws://127.0.0.1:1",
            "api-key",
            "secret-key",
            std::time::Duration::from_millis(10),
            Arc::new(|| 1_700_000_000_000),
        ));
        let account_summary = BybitAccountSummary::new(Arc::clone(&rest));
        let account = BybitAccount::new(Arc::clone(&rest), Arc::clone(&ws));
        let metadata = BybitMetadata::new(Arc::clone(&rest));
        let instrument = poise_engine::track::Instrument::new(Venue::Bybit, "BTCUSDT");

        let summary = account_summary.get_account_summary().await.unwrap();
        let capacity = account
            .get_account_capacity_snapshot(&instrument)
            .await
            .unwrap();
        let info = metadata.get_exchange_info(&instrument).await.unwrap();
        let server_time = metadata.get_server_time().await.unwrap();
        let requests = server.requests();

        assert_eq!(summary.available, 100.25);
        assert_eq!(capacity.max_increase_notional, 100.25);
        assert_eq!(info.instrument, instrument);
        assert_eq!(server_time.timestamp(), 1_700_000_000);
        assert_eq!(requests.len(), 4);
        assert!(
            !requests
                .iter()
                .any(|request| request.path.contains("not wired"))
        );
    }

    #[tokio::test]
    async fn connected_wires_market_and_private_ws_ports_to_bybit_ws_client() {
        let public_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public_addr = public_listener.local_addr().unwrap();
        let private_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let private_addr = private_listener.local_addr().unwrap();

        let market_messages = Arc::new(Mutex::new(Vec::new()));
        let market_messages_server = Arc::clone(&market_messages);
        tokio::spawn(async move {
            let (stream, _) = public_listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            if let Some(Ok(Message::Text(text))) = websocket.next().await {
                market_messages_server.lock().unwrap().push(text);
            }
            websocket
                .send(Message::Text(
                    r#"{"topic":"tickers.BTCUSDT","ts":1700000000000,"data":{"symbol":"BTCUSDT","markPrice":"64000.10","indexPrice":"63999.90"}}"#.to_string(),
                ))
                .await
                .unwrap();
            websocket.close(None).await.unwrap();
        });

        let private_messages = Arc::new(Mutex::new(Vec::new()));
        let private_messages_server = Arc::clone(&private_messages);
        tokio::spawn(async move {
            let (stream, _) = private_listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        private_messages_server.lock().unwrap().push(text.clone());
                        if private_messages_server.lock().unwrap().len() == 2 {
                            websocket
                                .send(Message::Text(r#"{"success":true,"op":"auth"}"#.to_string()))
                                .await
                                .unwrap();
                            websocket
                                .send(Message::Text(
                                    r#"{"success":true,"op":"subscribe"}"#.to_string(),
                                ))
                                .await
                                .unwrap();
                            websocket
                                .send(Message::Text(
                                    r#"{"topic":"order.linear","creationTime":1700000000000,"data":[{"symbol":"BTCUSDT","orderId":"123","orderLinkId":"client-1","side":"Buy","price":"64000.10","qty":"0.010","orderStatus":"New","positionIdx":0}]}"#.to_string(),
                                ))
                                .await
                                .unwrap();
                            websocket.close(None).await.unwrap();
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let rest = Arc::new(
            crate::rest::BybitRestClient::with_http_client_and_timestamp_provider(
                "http://127.0.0.1:1",
                "api-key",
                "secret-key",
                Arc::new(|| 1_700_000_000_000),
                reqwest::Client::new(),
            ),
        );
        let ws = Arc::new(BybitWsClient::with_test_params(
            format!("ws://{public_addr}"),
            format!("ws://{private_addr}"),
            "api-key",
            "secret-key",
            Duration::from_millis(10),
            Arc::new(|| 1_700_000_000_000),
        ));
        let connected = Connected::from_parts(
            Arc::new(BybitExecution::new(Arc::clone(&rest))),
            Arc::new(BybitMarketData::new(Arc::clone(&ws))),
            Arc::new(BybitAccountSummary::new(Arc::clone(&rest))),
            Arc::new(BybitAccount::new(Arc::clone(&rest), Arc::clone(&ws))),
            Arc::new(BybitMetadata::new(rest)),
        );
        let instrument = Instrument::new(Venue::Bybit, "BTCUSDT");

        let mut prices = connected
            .market_data()
            .subscribe_prices(&instrument)
            .await
            .unwrap();
        let mut user_data = connected.account().subscribe_user_data().await.unwrap();

        let tick = timeout(Duration::from_secs(1), prices.recv())
            .await
            .unwrap()
            .unwrap();
        let event = timeout(Duration::from_secs(1), user_data.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(tick.mark_price, 64000.10);
        assert_eq!(event.event_time.timestamp_millis(), 1_700_000_000_000);
        assert!(matches!(
            event.payload,
            poise_engine::ports::UserDataPayload::OrderUpdate(_)
        ));

        let market_messages = market_messages.lock().unwrap();
        let private_messages = private_messages.lock().unwrap();
        assert_eq!(market_messages.len(), 1);
        assert!(market_messages[0].contains("\"op\":\"subscribe\""));
        assert_eq!(private_messages.len(), 2);
        assert!(private_messages[0].contains("\"op\":\"auth\""));
        assert!(private_messages[1].contains("\"op\":\"subscribe\""));
        assert!(private_messages[1].contains("order.linear"));
        assert!(private_messages[1].contains("position.linear"));
    }

    #[tokio::test]
    async fn connected_wires_execution_port_to_bybit_rest_client() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"orderId":"12345","orderLinkId":"client-1"}}"#,
            ),
            MockResponse::json(200, r#"{"retCode":0,"retMsg":"OK","result":{}}"#),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[],"success":"1"}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"symbol":"BTCUSDT","side":"Buy","size":"0.010","avgPrice":"64000.10","unrealisedPnl":"1.25","positionIdx":0}]}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"retCode":0,"retMsg":"OK","result":{"list":[{"symbol":"BTCUSDT","orderId":"12345","orderLinkId":"client-1","side":"Buy","price":"64000.10","qty":"0.010","orderStatus":"New","positionIdx":0,"stopOrderType":"UNKNOWN"}]}}"#,
            ),
        ])
        .await;

        let rest = Arc::new(
            crate::rest::BybitRestClient::with_http_client_and_timestamp_provider(
                server.base_url(),
                "api-key",
                "secret-key",
                Arc::new(|| 1_700_000_000_000),
                reqwest::Client::new(),
            ),
        );
        let ws = Arc::new(BybitWsClient::with_test_params(
            "ws://127.0.0.1:1",
            "ws://127.0.0.1:1",
            "api-key",
            "secret-key",
            Duration::from_millis(10),
            Arc::new(|| 1_700_000_000_000),
        ));
        let connected = Connected::from_parts(
            Arc::new(BybitExecution::new(Arc::clone(&rest))),
            Arc::new(BybitMarketData::new(Arc::clone(&ws))),
            Arc::new(BybitAccountSummary::new(Arc::clone(&rest))),
            Arc::new(BybitAccount::new(Arc::clone(&rest), Arc::clone(&ws))),
            Arc::new(BybitMetadata::new(rest)),
        );
        let instrument = Instrument::new(Venue::Bybit, "BTCUSDT");

        let receipt = connected
            .execution()
            .submit_order(OrderRequest {
                instrument: instrument.clone(),
                side: Side::Buy,
                price: 64000.10,
                quantity: 0.010,
                client_order_id: "client-1".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();
        let _ = connected
            .execution()
            .cancel_order(&instrument, "12345")
            .await
            .unwrap();
        let _ = connected.execution().cancel_all(&instrument).await.unwrap();
        let position = connected
            .execution()
            .get_position(&instrument)
            .await
            .unwrap();
        let open_orders = connected
            .execution()
            .get_open_orders(&instrument)
            .await
            .unwrap();

        assert_eq!(receipt.status, OrderStatus::Submitting);
        assert_eq!(position.qty, 0.010);
        assert_eq!(open_orders.len(), 1);
        assert_eq!(open_orders[0].client_order_id, "client-1");

        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0].path, "/v5/order/create");
        assert_eq!(requests[1].path, "/v5/order/cancel");
        assert_eq!(requests[2].path, "/v5/order/cancel-all");
        assert_eq!(
            requests[3].path,
            "/v5/position/list?category=linear&symbol=BTCUSDT"
        );
        assert_eq!(
            requests[4].path,
            "/v5/order/realtime?category=linear&symbol=BTCUSDT&orderFilter=Order"
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
        body: String,
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
