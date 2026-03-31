use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use poise_engine::ports::{
    AccountMarginSnapshot, ExchangeInfo, ExchangeOrder, ExchangePort, MarketDataPort, OrderReceipt,
    OrderRequest, Position, PriceTick, UserDataEvent,
};
use poise_engine::track::Instrument;

use crate::{rest::BinanceRestClient, websocket::BinanceWsClient};

pub struct BinanceAdapter {
    #[allow(dead_code)]
    rest: Arc<BinanceRestClient>,
    #[allow(dead_code)]
    ws: BinanceWsClient,
}

impl BinanceAdapter {
    pub fn new(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        rest_base_url: impl Into<String>,
        ws_base_url: impl Into<String>,
    ) -> Self {
        let rest = Arc::new(BinanceRestClient::new(rest_base_url, api_key, api_secret));
        let ws = BinanceWsClient::new(Arc::clone(&rest), ws_base_url);

        Self { rest, ws }
    }
}

#[async_trait]
impl ExchangePort for BinanceAdapter {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        self.rest.new_order(&req).await
    }

    async fn cancel_order(&self, instrument: &Instrument, order_id: &str) -> Result<()> {
        self.rest.cancel_order(&instrument.symbol, order_id).await?;
        Ok(())
    }

    async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
        self.rest.cancel_all_orders(&instrument.symbol).await?;
        Ok(())
    }

    async fn get_position(&self, instrument: &Instrument) -> Result<Position> {
        self.rest.get_position(&instrument.symbol).await
    }

    async fn get_open_orders(&self, instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
        self.rest.get_open_orders(&instrument.symbol).await
    }

    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
        self.rest.get_exchange_info(&instrument.symbol).await
    }

    async fn get_account_margin_snapshot(
        &self,
        instrument: &Instrument,
    ) -> Result<AccountMarginSnapshot> {
        self.rest.get_account_margin_snapshot(&instrument.symbol).await
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
        self.rest.get_server_time().await
    }
}

#[async_trait]
impl MarketDataPort for BinanceAdapter {
    async fn subscribe_prices(&self, instrument: &Instrument) -> Result<mpsc::Receiver<PriceTick>> {
        self.ws.subscribe_prices(instrument).await
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        self.ws.subscribe_user_data().await
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

    use poise_core::types::Side;
    use poise_engine::track::{Instrument, Venue};

    use super::*;

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
        let adapter = BinanceAdapter::new(
            "api-key",
            "secret-key",
            server.base_url(),
            "ws://127.0.0.1:1",
        );

        let receipt = adapter
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
        let adapter = BinanceAdapter::new(
            "api-key",
            "secret-key",
            server.base_url(),
            "ws://127.0.0.1:1",
        );

        adapter
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
        let adapter = BinanceAdapter::new(
            "api-key",
            "secret-key",
            server.base_url(),
            "ws://127.0.0.1:1",
        );

        adapter
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
        let adapter = BinanceAdapter::new(
            "api-key",
            "secret-key",
            server.base_url(),
            "ws://127.0.0.1:1",
        );

        adapter
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
        let adapter = BinanceAdapter::new(
            "api-key",
            "secret-key",
            server.base_url(),
            "ws://127.0.0.1:1",
        );

        let position = adapter
            .get_position(&Instrument::new(Venue::Binance, "BTCUSDT"))
            .await
            .unwrap();

        assert_eq!(position.instrument.symbol, "BTCUSDT");
        assert_eq!(position.qty, 0.25);
        assert_eq!(position.avg_price, 65000.5);
    }

    #[tokio::test]
    async fn account_margin_snapshot_calls_rest_and_converts_payload() {
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
        let adapter = BinanceAdapter::new(
            "api-key",
            "secret-key",
            server.base_url(),
            "ws://127.0.0.1:1",
        );

        let snapshot = adapter
            .get_account_margin_snapshot(&Instrument::new(Venue::Binance, "BTCUSDT"))
            .await
            .unwrap();
        let requests = server.requests().await;

        assert_eq!(snapshot.venue, Venue::Binance);
        assert_eq!(snapshot.available_balance, 100.5);
        assert_eq!(snapshot.total_wallet_balance, 120.75);
        assert!((snapshot.max_increase_notional - 2010.0).abs() < f64::EPSILON);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].path.starts_with("/fapi/v2/account?"));
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

        let adapter = BinanceAdapter::new(
            "api-key",
            "secret-key",
            "http://127.0.0.1:1",
            format!("ws://{}", address),
        );

        let mut receiver = adapter
            .subscribe_prices(&Instrument::new(Venue::Binance, "BTCUSDT"))
            .await
            .unwrap();
        let tick = timeout(std::time::Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(tick.instrument.symbol, "BTCUSDT");
        assert_eq!(tick.mark_price, 64000.10);
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
