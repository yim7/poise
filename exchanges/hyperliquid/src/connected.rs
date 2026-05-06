use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::mpsc;

use poise_core::track::Instrument;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, AccountSummaryPort, AccountSummarySnapshot, ExchangeInfo,
    ExchangeOpenOrderSnapshot, ExchangePorts, ExecutionPort, ExecutionPortError, ExecutionResult,
    MarketDataPort, MarketDataTick, MetadataPort, OrderReceipt, OrderRequest, Position,
    UserDataEvent,
};

use crate::{
    Config, client_order_id::ClientOrderIdMapper, rest::client::HyperliquidRestClient,
    rest::error::HyperliquidRestError, ws::HyperliquidWsClient,
};

const DEFAULT_CAPACITY_LEVERAGE: u32 = 10;

pub async fn connect(config: &Config) -> Result<ExchangePorts> {
    let credentials = config.credentials()?;
    let client_order_ids = ClientOrderIdMapper::shared();
    let ws = Arc::new(HyperliquidWsClient::new_with_client_order_id_mapper(
        config.endpoints().ws_url().to_string(),
        credentials.wallet_address().to_string(),
        Arc::clone(&client_order_ids),
    ));
    let rest = Arc::new(HyperliquidRestClient::new_with_client_order_id_mapper(
        config,
        client_order_ids,
    )?);
    let execution: Arc<dyn ExecutionPort> = rest.clone();
    let market_data: Arc<dyn MarketDataPort> = ws.clone();
    let account_summary: Arc<dyn AccountSummaryPort> = rest.clone();
    let metadata: Arc<dyn MetadataPort> = rest.clone();

    Ok(ExchangePorts::new(
        execution,
        market_data,
        account_summary,
        Arc::new(HyperliquidAccount {
            rest: Arc::clone(&rest),
            ws,
        }),
        metadata,
    ))
}

struct HyperliquidAccount {
    rest: Arc<HyperliquidRestClient>,
    ws: Arc<HyperliquidWsClient>,
}

fn map_execution_error(error: anyhow::Error) -> ExecutionPortError {
    if let Some(kind) = error
        .downcast_ref::<HyperliquidRestError>()
        .and_then(HyperliquidRestError::execution_error_kind)
    {
        return ExecutionPortError::new(kind, error);
    }

    ExecutionPortError::from(error)
}

#[async_trait]
impl ExecutionPort for HyperliquidRestClient {
    async fn submit_order(&self, req: OrderRequest) -> ExecutionResult<OrderReceipt> {
        HyperliquidRestClient::submit_order(self, req)
            .await
            .map_err(map_execution_error)
    }

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        order_id: &str,
    ) -> ExecutionResult<OrderReceipt> {
        HyperliquidRestClient::cancel_order(self, &instrument.symbol, order_id)
            .await
            .map_err(map_execution_error)
    }

    async fn cancel_all(&self, instrument: &Instrument) -> ExecutionResult<()> {
        HyperliquidRestClient::cancel_all(self, &instrument.symbol)
            .await
            .map_err(map_execution_error)
    }

    async fn get_position(&self, instrument: &Instrument) -> ExecutionResult<Position> {
        HyperliquidRestClient::get_position(self, &instrument.symbol)
            .await
            .map_err(map_execution_error)
    }

    async fn get_open_orders(
        &self,
        instrument: &Instrument,
    ) -> ExecutionResult<ExchangeOpenOrderSnapshot> {
        HyperliquidRestClient::get_open_orders(self, &instrument.symbol)
            .await
            .map(ExchangeOpenOrderSnapshot::from_complete_exchange_query)
            .map_err(map_execution_error)
    }
}

#[async_trait]
impl MarketDataPort for HyperliquidWsClient {
    async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        self.subscribe_prices(instrument).await
    }
}

#[async_trait]
impl AccountSummaryPort for HyperliquidRestClient {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        self.get_account_summary().await
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
impl MetadataPort for HyperliquidRestClient {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
        self.get_exchange_info(&instrument.symbol).await
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

        assert_execution_port::<HyperliquidRestClient>();
    }
}
