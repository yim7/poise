use std::sync::Arc;

use anyhow::{Result, anyhow};
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
        api_key,
        api_secret,
    ));
    let ws = Arc::new(BybitWsClient::new(Arc::clone(&rest), deployment));

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

fn not_wired(port_name: &str) -> anyhow::Error {
    anyhow!("bybit {port_name} is not wired yet")
}

#[async_trait]
impl ExecutionPort for BybitExecution {
    async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
        Err(not_wired("execution"))
    }

    async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
        Err(not_wired("execution"))
    }

    async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
        Err(not_wired("execution"))
    }

    async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
        Err(not_wired("execution"))
    }

    async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
        Err(not_wired("execution"))
    }
}

#[async_trait]
impl MarketDataPort for BybitMarketData {
    async fn subscribe_prices(
        &self,
        _instrument: &Instrument,
    ) -> Result<mpsc::Receiver<PriceTick>> {
        Err(not_wired("market data"))
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
        Err(not_wired("account"))
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
    use std::sync::Mutex;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use poise_engine::track::Venue;

    use super::*;
    use crate::Deployment;

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
        let account_summary = BybitAccountSummary::new(Arc::clone(&rest));
        let account = BybitAccount::new(
            Arc::clone(&rest),
            Arc::new(BybitWsClient::new(Arc::clone(&rest), Deployment::Mainnet)),
        );
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
