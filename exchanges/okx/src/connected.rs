use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use poise_core::track::Instrument;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, AccountSummaryPort, AccountSummarySnapshot, ExchangeInfo,
    ExchangeOpenOrderSnapshot, ExchangePorts, ExecutionPort, ExecutionPortError, ExecutionResult,
    MarketDataPort, MarketDataTick, MetadataPort, OrderReceipt, OrderRequest, Position,
    UserDataEvent,
};

use crate::{Config, rest::client::OkxRestClient, rest::error::OkxRestError, ws::OkxWsClient};

pub async fn connect(config: &Config) -> Result<ExchangePorts> {
    let credentials = config.credentials()?;
    let endpoints = config.endpoints();
    Ok(ports_from_clients(
        Arc::new(OkxRestClient::new(config)?),
        Arc::new(OkxWsClient::new(
            endpoints.public_ws_url(),
            endpoints.private_ws_url(),
            credentials,
        )),
    ))
}

fn ports_from_clients(rest: Arc<OkxRestClient>, ws: Arc<OkxWsClient>) -> ExchangePorts {
    let execution: Arc<dyn ExecutionPort> = rest.clone();
    let market_data: Arc<dyn MarketDataPort> = ws.clone();
    let account_summary: Arc<dyn AccountSummaryPort> = rest.clone();
    let metadata: Arc<dyn MetadataPort> = rest.clone();

    ExchangePorts::new(
        execution,
        market_data,
        account_summary,
        Arc::new(OkxAccount {
            rest: Arc::clone(&rest),
            ws,
        }),
        metadata,
    )
}

struct OkxAccount {
    rest: Arc<OkxRestClient>,
    ws: Arc<OkxWsClient>,
}

fn map_execution_error(error: anyhow::Error) -> ExecutionPortError {
    if let Some(kind) = error
        .downcast_ref::<OkxRestError>()
        .and_then(OkxRestError::execution_error_kind)
    {
        return ExecutionPortError::new(kind, error);
    }

    ExecutionPortError::from(error)
}

#[async_trait]
impl ExecutionPort for OkxRestClient {
    async fn submit_order(&self, req: OrderRequest) -> ExecutionResult<OrderReceipt> {
        OkxRestClient::submit_order(self, req)
            .await
            .map_err(map_execution_error)
    }

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        order_id: &str,
    ) -> ExecutionResult<OrderReceipt> {
        OkxRestClient::cancel_order(self, &instrument.symbol, order_id)
            .await
            .map_err(map_execution_error)
    }

    async fn cancel_all(&self, instrument: &Instrument) -> ExecutionResult<()> {
        OkxRestClient::cancel_all(self, &instrument.symbol)
            .await
            .map_err(map_execution_error)
    }

    async fn get_position(&self, instrument: &Instrument) -> ExecutionResult<Position> {
        OkxRestClient::get_position(self, &instrument.symbol)
            .await
            .map_err(map_execution_error)
    }

    async fn get_open_orders(
        &self,
        instrument: &Instrument,
    ) -> ExecutionResult<ExchangeOpenOrderSnapshot> {
        OkxRestClient::get_open_orders(self, &instrument.symbol)
            .await
            .map(ExchangeOpenOrderSnapshot::from_complete_exchange_query)
            .map_err(map_execution_error)
    }
}

#[async_trait]
impl MarketDataPort for OkxWsClient {
    async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        self.subscribe_prices(instrument).await
    }
}

#[async_trait]
impl AccountSummaryPort for OkxRestClient {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        self.get_account_summary().await
    }

    async fn get_available_balance(&self, instrument: &Instrument) -> Result<f64> {
        self.get_available_balance(&instrument.symbol).await
    }
}

#[async_trait]
impl AccountPort for OkxAccount {
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
impl MetadataPort for OkxRestClient {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
        self.get_exchange_info(&instrument.symbol).await
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
        self.get_server_time().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use chrono::{DateTime, Utc};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::Duration,
    };

    use super::*;
    use crate::rest::client::OkxRestClient;
    use crate::{Config, Deployment};

    #[tokio::test]
    async fn connected_exposes_all_required_ports() {
        let config = Config {
            deployment: Deployment::Demo,
            api_key: Some("demo-key".to_string()),
            api_secret: Some("demo-secret".to_string()),
            passphrase: Some("demo-passphrase".to_string()),
        };

        let connected: ExchangePorts = connect(&config).await.unwrap();

        let _execution = connected.execution();
        let _market_data = connected.market_data();
        let _account_summary = connected.account_summary();
        let _account = connected.account();
        let _metadata = connected.metadata();
    }

    #[test]
    fn rest_client_implements_execution_port_directly() {
        fn assert_execution_port<T: ExecutionPort>() {}

        assert_execution_port::<OkxRestClient>();
    }

    #[test]
    fn ws_client_implements_market_data_port_directly() {
        fn assert_market_data_port<T: MarketDataPort>() {}

        assert_market_data_port::<OkxWsClient>();
    }

    #[tokio::test]
    async fn connected_account_summary_port_routes_rest_client() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"totalEq":"12500.5","details":[{"ccy":"USDT","availEq":"9800.25","upl":"-120.75"}]}]}"#,
        )])
        .await;
        let rest = Arc::new(OkxRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            Config {
                deployment: Deployment::Demo,
                api_key: Some("api-key".to_string()),
                api_secret: Some("secret-key".to_string()),
                passphrase: Some("passphrase".to_string()),
            }
            .credentials()
            .unwrap(),
            true,
            Arc::new(fixed_datetime),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        ));

        let connected = ports_from_clients(rest, Arc::new(dummy_ws_client()));
        let summary = connected
            .account_summary()
            .get_account_summary()
            .await
            .unwrap();

        assert_eq!(summary.equity, 12_500.5);
        assert_eq!(summary.available, 9_800.25);
        assert_eq!(server.requests()[0].path, "/api/v5/account/balance");
    }

    fn fixed_datetime() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2020-12-08T09:08:57.715Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn dummy_ws_client() -> OkxWsClient {
        OkxWsClient::with_test_params(
            "ws://127.0.0.1:1",
            "ws://127.0.0.1:1",
            "api-key",
            "secret-key",
            "passphrase",
            Duration::from_millis(10),
            Arc::new(|| 1_700_000_000),
        )
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
                    let mut buffer = Vec::new();
                    loop {
                        let mut chunk = [0_u8; 4096];
                        let read = socket.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if request_complete(&buffer) {
                            break;
                        }
                    }
                    if buffer.is_empty() {
                        break;
                    }
                    stored_requests
                        .lock()
                        .unwrap()
                        .push(parse_request(&String::from_utf8_lossy(&buffer)));

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
        path: String,
        headers: HashMap<String, String>,
    }

    fn request_complete(buffer: &[u8]) -> bool {
        let request_text = String::from_utf8_lossy(buffer);
        let Some((head, body)) = request_text.split_once("\r\n\r\n") else {
            return false;
        };
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        body.len() >= content_length
    }

    fn parse_request(raw: &str) -> RecordedRequest {
        let (head, _) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap();
        let path = request_line.split_whitespace().nth(1).unwrap().to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        RecordedRequest { path, headers }
    }
}
