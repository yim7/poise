use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::mpsc;

use poise_core::track::Instrument;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, AccountSummaryPort, AccountSummarySnapshot, ExchangeInfo,
    ExchangeOpenOrderSnapshot, ExecutionPort, MarketDataPort, MarketDataTick, MetadataPort,
    OrderReceipt, OrderRequest, Position, UserDataEvent,
};

use crate::Config;

pub async fn connect(config: &Config) -> Result<Connected> {
    let _credentials = config.credentials()?;
    Ok(Connected::from_parts(
        Arc::new(OkxPendingExecution),
        Arc::new(OkxPendingMarketData),
        Arc::new(OkxPendingAccountSummary),
        Arc::new(OkxPendingAccount),
        Arc::new(OkxPendingMetadata),
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

struct OkxPendingExecution;
struct OkxPendingMarketData;
struct OkxPendingAccountSummary;
struct OkxPendingAccount;
struct OkxPendingMetadata;

#[async_trait]
impl ExecutionPort for OkxPendingExecution {
    async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
        Err(anyhow!(
            "OKX order submission is pending REST client wiring"
        ))
    }

    async fn cancel_order(
        &self,
        _instrument: &Instrument,
        _order_id: &str,
    ) -> Result<OrderReceipt> {
        Err(anyhow!(
            "OKX order cancellation is pending REST client wiring"
        ))
    }

    async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
        Err(anyhow!("OKX cancel-all is pending REST client wiring"))
    }

    async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
        Err(anyhow!("OKX position query is pending REST client wiring"))
    }

    async fn get_open_orders(&self, _instrument: &Instrument) -> Result<ExchangeOpenOrderSnapshot> {
        Err(anyhow!(
            "OKX open-order query is pending REST client wiring"
        ))
    }
}

#[async_trait]
impl MarketDataPort for OkxPendingMarketData {
    async fn subscribe_prices(
        &self,
        _instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        Err(anyhow!(
            "OKX market data stream is pending WebSocket wiring"
        ))
    }
}

#[async_trait]
impl AccountSummaryPort for OkxPendingAccountSummary {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        Err(anyhow!(
            "OKX account summary query is pending REST client wiring"
        ))
    }
}

#[async_trait]
impl AccountPort for OkxPendingAccount {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot> {
        Err(anyhow!(
            "OKX account capacity query is pending REST client wiring"
        ))
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        Err(anyhow!("OKX user data stream is pending WebSocket wiring"))
    }
}

#[async_trait]
impl MetadataPort for OkxPendingMetadata {
    async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
        Err(anyhow!(
            "OKX exchange-info query is pending REST client wiring"
        ))
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        Ok(Utc::now())
    }
}
