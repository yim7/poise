use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::mpsc;

use poise_core::track::Instrument;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, AccountSummaryPort, AccountSummarySnapshot, ExchangeInfo,
    ExchangeOpenOrderSnapshot, ExecutionPort, MarketDataPort, MarketDataTick, MetadataPort,
    OrderReceipt, OrderRequest, Position, UserDataEvent,
};

use crate::{
    Config, client_order_id::ClientOrderIdMapper, rest::client::HyperliquidRestClient,
    ws::HyperliquidWsClient,
};

const DEFAULT_CAPACITY_LEVERAGE: u32 = 10;

pub async fn connect(config: &Config) -> Result<Connected> {
    let credentials = config.credentials()?;
    let client_order_ids = ClientOrderIdMapper::shared();
    let ws = Arc::new(HyperliquidWsClient::new_with_client_order_id_mapper(
        config.endpoints().ws_url().to_string(),
        credentials.wallet_address().to_string(),
        Arc::clone(&client_order_ids),
    ));
    Ok(Connected::from_rest_client(
        Arc::new(HyperliquidRestClient::new_with_client_order_id_mapper(
            config,
            client_order_ids,
        )?),
        ws,
    ))
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
    fn from_rest_client(rest: Arc<HyperliquidRestClient>, ws: Arc<HyperliquidWsClient>) -> Self {
        Self::from_parts(
            Arc::new(HyperliquidExecution::new(Arc::clone(&rest))),
            Arc::new(HyperliquidMarketData::new(Arc::clone(&ws))),
            Arc::new(HyperliquidAccountSummary::new(Arc::clone(&rest))),
            Arc::new(HyperliquidAccount::new(Arc::clone(&rest), ws)),
            Arc::new(HyperliquidMetadata::new(rest)),
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

struct HyperliquidExecution {
    rest: Arc<HyperliquidRestClient>,
}

impl HyperliquidExecution {
    fn new(rest: Arc<HyperliquidRestClient>) -> Self {
        Self { rest }
    }
}

struct HyperliquidMarketData {
    ws: Arc<HyperliquidWsClient>,
}

impl HyperliquidMarketData {
    fn new(ws: Arc<HyperliquidWsClient>) -> Self {
        Self { ws }
    }
}

struct HyperliquidAccountSummary {
    rest: Arc<HyperliquidRestClient>,
}

impl HyperliquidAccountSummary {
    fn new(rest: Arc<HyperliquidRestClient>) -> Self {
        Self { rest }
    }
}

struct HyperliquidAccount {
    rest: Arc<HyperliquidRestClient>,
    ws: Arc<HyperliquidWsClient>,
}

impl HyperliquidAccount {
    fn new(rest: Arc<HyperliquidRestClient>, ws: Arc<HyperliquidWsClient>) -> Self {
        Self { rest, ws }
    }
}

struct HyperliquidMetadata {
    rest: Arc<HyperliquidRestClient>,
}

impl HyperliquidMetadata {
    fn new(rest: Arc<HyperliquidRestClient>) -> Self {
        Self { rest }
    }
}

#[async_trait]
impl ExecutionPort for HyperliquidExecution {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        self.rest.submit_order(req).await
    }

    async fn cancel_order(&self, instrument: &Instrument, order_id: &str) -> Result<OrderReceipt> {
        self.rest.cancel_order(&instrument.symbol, order_id).await
    }

    async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
        self.rest.cancel_all(&instrument.symbol).await
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
impl MarketDataPort for HyperliquidMarketData {
    async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        self.ws.subscribe_prices(instrument).await
    }
}

#[async_trait]
impl AccountSummaryPort for HyperliquidAccountSummary {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        self.rest.get_account_summary().await
    }
}

#[async_trait]
impl AccountPort for HyperliquidAccount {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot> {
        self.rest
            .get_account_capacity_snapshot(DEFAULT_CAPACITY_LEVERAGE)
            .await
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        self.ws.subscribe_user_data().await
    }
}

#[async_trait]
impl MetadataPort for HyperliquidMetadata {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
        self.rest.get_exchange_info(&instrument.symbol).await
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        Ok(Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connected_exposes_all_required_ports() {
        let config = Config {
            deployment: crate::Deployment::Testnet,
            private_key: Some(
                "0xe908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e".to_string(),
            ),
            wallet_address: Some("0x2222222222222222222222222222222222222222".to_string()),
            vault_address: None,
        };

        let connected = connect(&config).await.unwrap();

        let _execution = connected.execution();
        let _market_data = connected.market_data();
        let _account_summary = connected.account_summary();
        let _account = connected.account();
        let _metadata = connected.metadata();
    }
}
