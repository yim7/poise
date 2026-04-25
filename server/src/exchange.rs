use std::sync::Arc;

use poise_engine::ports::{
    AccountPort, AccountSummaryPort, ExecutionPort, MarketDataPort, MetadataPort,
};
use poise_engine::track::Venue;

#[derive(Clone)]
pub struct Exchange {
    #[cfg(test)]
    venue: Venue,
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
}

impl Exchange {
    pub fn new(
        venue: Venue,
        execution: Arc<dyn ExecutionPort>,
        market_data: Arc<dyn MarketDataPort>,
        account_summary: Arc<dyn AccountSummaryPort>,
        account: Arc<dyn AccountPort>,
        metadata: Arc<dyn MetadataPort>,
    ) -> Self {
        #[cfg(not(test))]
        let _ = venue;

        Self {
            #[cfg(test)]
            venue,
            execution,
            market_data,
            account_summary,
            account,
            metadata,
        }
    }

    #[cfg(test)]
    pub fn venue(&self) -> Venue {
        self.venue
    }

    #[cfg(test)]
    pub fn execution(&self) -> &dyn ExecutionPort {
        self.execution.as_ref()
    }

    #[cfg(test)]
    pub fn market_data(&self) -> &dyn MarketDataPort {
        self.market_data.as_ref()
    }

    #[cfg(test)]
    pub fn account_summary(&self) -> &dyn AccountSummaryPort {
        self.account_summary.as_ref()
    }

    #[cfg(test)]
    pub fn account(&self) -> &dyn AccountPort {
        self.account.as_ref()
    }

    pub fn metadata(&self) -> &dyn MetadataPort {
        self.metadata.as_ref()
    }

    pub(crate) fn execution_port(&self) -> Arc<dyn ExecutionPort> {
        Arc::clone(&self.execution)
    }

    pub(crate) fn market_data_port(&self) -> Arc<dyn MarketDataPort> {
        Arc::clone(&self.market_data)
    }

    pub(crate) fn account_summary_port(&self) -> Arc<dyn AccountSummaryPort> {
        Arc::clone(&self.account_summary)
    }

    pub(crate) fn account_port(&self) -> Arc<dyn AccountPort> {
        Arc::clone(&self.account)
    }

    pub(crate) fn metadata_port(&self) -> Arc<dyn MetadataPort> {
        Arc::clone(&self.metadata)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::Utc;
    use tokio::sync::mpsc;

    use poise_engine::ports::{
        AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo, ExchangeOpenOrderSnapshot,
        MarketDataTick, OrderReceipt, OrderRequest, Position, UserDataEvent,
    };
    use poise_engine::track::Instrument;

    use super::*;

    #[derive(Default)]
    struct FakeExecutionPort;

    #[async_trait]
    impl ExecutionPort for FakeExecutionPort {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            unreachable!("not used in tests")
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> Result<OrderReceipt> {
            unreachable!("not used in tests")
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            unreachable!("not used in tests")
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            unreachable!("not used in tests")
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> Result<ExchangeOpenOrderSnapshot> {
            unreachable!("not used in tests")
        }
    }

    #[derive(Default)]
    struct FakeMarketDataPort;

    #[async_trait]
    impl MarketDataPort for FakeMarketDataPort {
        async fn subscribe_prices(
            &self,
            _instrument: &Instrument,
        ) -> Result<mpsc::Receiver<MarketDataTick>> {
            unreachable!("not used in tests")
        }
    }

    #[derive(Default)]
    struct FakeAccountSummaryPort;

    #[async_trait]
    impl AccountSummaryPort for FakeAccountSummaryPort {
        async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
            Ok(AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: Utc::now(),
            })
        }
    }

    #[derive(Default)]
    struct FakeAccountPort;

    #[async_trait]
    impl AccountPort for FakeAccountPort {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> Result<AccountCapacitySnapshot> {
            Ok(AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let (_sender, receiver) = mpsc::channel(1);
            Ok(receiver)
        }
    }

    #[derive(Default)]
    struct FakeMetadataPort;

    #[async_trait]
    impl MetadataPort for FakeMetadataPort {
        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            unreachable!("not used in tests")
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }

    #[test]
    fn exchange_retains_venue_and_exposes_stable_ports() {
        let exchange = Exchange::new(
            Venue::Binance,
            Arc::new(FakeExecutionPort),
            Arc::new(FakeMarketDataPort),
            Arc::new(FakeAccountSummaryPort),
            Arc::new(FakeAccountPort),
            Arc::new(FakeMetadataPort),
        );

        assert_eq!(exchange.venue(), Venue::Binance);
        let _execution: &dyn ExecutionPort = exchange.execution();
        let _market_data: &dyn MarketDataPort = exchange.market_data();
        let _account_summary: &dyn AccountSummaryPort = exchange.account_summary();
        let _account: &dyn AccountPort = exchange.account();
        let _metadata: &dyn MetadataPort = exchange.metadata();
    }
}
