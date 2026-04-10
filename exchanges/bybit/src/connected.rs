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
        Err(not_wired("account summary"))
    }
}

#[async_trait]
impl AccountPort for BybitAccount {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot> {
        Err(not_wired("account"))
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        Err(not_wired("account"))
    }
}

#[async_trait]
impl MetadataPort for BybitMetadata {
    async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
        Err(not_wired("metadata"))
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
        Err(not_wired("metadata"))
    }
}

#[cfg(test)]
mod tests {
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
}
