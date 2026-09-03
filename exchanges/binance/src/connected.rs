use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use poise_application::exchange::{
    AccountPort, AccountSummaryPort, AccountSummarySnapshot, ExchangeInfo,
    ExchangeOpenOrderSnapshot, ExchangePorts, ExecutionPort, ExecutionPortError,
    ExecutionPortErrorKind, ExecutionResult, MarketDataPort, MarketDataTick, MetadataPort,
    OrderReceipt, OrderRequest, PnlHistoryWindow, Position, RecentTrackPnlRecords, UserDataEvent,
};
use poise_core::track::Instrument;

use crate::{
    Config,
    rest::{BinanceRestClient, BinanceRestError, BinanceSubmitOutcomeUnknown},
    ws::BinanceWsClient,
};

pub async fn connect(config: &Config) -> Result<ExchangePorts> {
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

    Ok(ports_from_clients(rest, ws))
}

fn ports_from_clients(rest: Arc<BinanceRestClient>, ws: Arc<BinanceWsClient>) -> ExchangePorts {
    let execution: Arc<dyn ExecutionPort> = rest.clone();
    let market_data: Arc<dyn MarketDataPort> = ws.clone();
    let account_summary: Arc<dyn AccountSummaryPort> = rest.clone();
    let metadata: Arc<dyn MetadataPort> = rest.clone();

    ExchangePorts::new(
        execution,
        market_data,
        account_summary,
        Arc::new(BinanceAccount {
            rest: Arc::clone(&rest),
            ws,
        }),
        metadata,
    )
}

fn map_execution_error(error: anyhow::Error) -> ExecutionPortError {
    if let Some(kind) = error
        .downcast_ref::<BinanceRestError>()
        .and_then(BinanceRestError::execution_error_kind)
    {
        return ExecutionPortError::new(kind, error);
    }

    ExecutionPortError::from(error)
}

fn map_submit_execution_error(error: anyhow::Error) -> ExecutionPortError {
    if error
        .downcast_ref::<BinanceSubmitOutcomeUnknown>()
        .is_some()
    {
        return ExecutionPortError::new(ExecutionPortErrorKind::SubmitOutcomeUnknown, error);
    }

    if let Some(kind) = error
        .downcast_ref::<BinanceRestError>()
        .and_then(BinanceRestError::execution_error_kind)
        .filter(|kind| *kind != ExecutionPortErrorKind::CancelOutcomeUnknown)
    {
        return ExecutionPortError::new(kind, error);
    }

    ExecutionPortError::from(error)
}

struct BinanceAccount {
    rest: Arc<BinanceRestClient>,
    ws: Arc<BinanceWsClient>,
}

#[async_trait]
impl AccountSummaryPort for BinanceRestClient {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        self.get_account_summary().await
    }
}

#[async_trait]
impl ExecutionPort for BinanceRestClient {
    async fn submit_order(&self, req: OrderRequest) -> ExecutionResult<OrderReceipt> {
        BinanceRestClient::new_order(self, &req)
            .await
            .map_err(map_submit_execution_error)
    }

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        order_id: &str,
    ) -> ExecutionResult<OrderReceipt> {
        BinanceRestClient::cancel_order(self, &instrument.symbol, order_id)
            .await
            .map_err(map_execution_error)
    }

    async fn cancel_all(&self, instrument: &Instrument) -> ExecutionResult<()> {
        BinanceRestClient::cancel_all_orders(self, &instrument.symbol)
            .await
            .map_err(map_execution_error)
    }

    async fn get_position(&self, instrument: &Instrument) -> ExecutionResult<Position> {
        BinanceRestClient::get_position(self, &instrument.symbol)
            .await
            .map_err(map_execution_error)
    }

    async fn get_open_orders(
        &self,
        instrument: &Instrument,
    ) -> ExecutionResult<ExchangeOpenOrderSnapshot> {
        BinanceRestClient::get_open_orders(self, &instrument.symbol)
            .await
            .map(ExchangeOpenOrderSnapshot::from_complete_exchange_query)
            .map_err(map_execution_error)
    }
}

#[async_trait]
impl AccountPort for BinanceAccount {
    async fn get_recent_track_pnl_records(
        &self,
        instrument: &Instrument,
        required_window: &PnlHistoryWindow,
    ) -> Result<RecentTrackPnlRecords> {
        self.rest
            .get_recent_track_pnl_records(&instrument.symbol, required_window)
            .await
            .map(RecentTrackPnlRecords::Available)
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        self.ws.subscribe_user_data().await
    }
}

#[async_trait]
impl MetadataPort for BinanceRestClient {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo> {
        self.get_exchange_info(&instrument.symbol).await
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>> {
        self.get_server_time().await
    }
}

#[async_trait]
impl MarketDataPort for BinanceWsClient {
    async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        self.subscribe_prices(instrument).await
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

    use poise_application::exchange::ExecutionPortErrorKind;
    use poise_core::instrument::{Amount, Asset, InstrumentSpec, PositionQty};
    use poise_core::{
        track::{Instrument, Venue},
        types::Side,
    };

    use super::*;

    struct FakeExecutionPort;
    struct FakeMarketDataPort;
    struct FakeAccountSummaryPort;
    struct FakeAccountPort;
    struct FakeMetadataPort;

    #[async_trait]
    impl ExecutionPort for FakeExecutionPort {
        async fn submit_order(
            &self,
            _req: OrderRequest,
        ) -> poise_application::exchange::ExecutionResult<OrderReceipt> {
            unreachable!("not used in test")
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> poise_application::exchange::ExecutionResult<OrderReceipt> {
            unreachable!("not used in test")
        }

        async fn cancel_all(
            &self,
            _instrument: &Instrument,
        ) -> poise_application::exchange::ExecutionResult<()> {
            unreachable!("not used in test")
        }

        async fn get_position(
            &self,
            _instrument: &Instrument,
        ) -> poise_application::exchange::ExecutionResult<Position> {
            unreachable!("not used in test")
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> poise_application::exchange::ExecutionResult<ExchangeOpenOrderSnapshot> {
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
        async fn get_recent_track_pnl_records(
            &self,
            _instrument: &Instrument,
            _required_window: &PnlHistoryWindow,
        ) -> Result<RecentTrackPnlRecords> {
            Ok(RecentTrackPnlRecords::Unsupported)
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

    fn build_test_connected() -> ExchangePorts {
        build_test_connected_with_urls("http://127.0.0.1:18080", "ws://127.0.0.1:19080")
    }

    fn build_test_connected_with_urls(
        rest_base_url: impl Into<String>,
        ws_base_url: impl Into<String>,
    ) -> ExchangePorts {
        let rest = Arc::new(BinanceRestClient::new(
            rest_base_url,
            "api-key",
            "secret-key",
        ));
        rest.instrument_registry().upsert(
            InstrumentSpec::try_linear(
                Instrument::new(Venue::Binance, "BTCUSDT"),
                Asset::try_new("BTC").unwrap(),
                Asset::try_new("USDT").unwrap(),
                Asset::try_new("USDT").unwrap(),
                Default::default(),
                0.1,
                0.001,
                0.001,
                Amount::new(5.0, Asset::try_new("USDT").unwrap()).unwrap(),
            )
            .unwrap(),
        );
        let ws_base_url = ws_base_url.into();
        let ws = Arc::new(BinanceWsClient::with_ws_routes(
            Arc::clone(&rest),
            ws_base_url.clone(),
            ws_base_url.clone(),
            ws_base_url,
        ));
        ports_from_clients(rest, ws)
    }

    fn submit_request(client_order_id: &str) -> OrderRequest {
        OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            side: Side::Buy,
            price: 65_000.0,
            quantity: PositionQty::new(0.1).unwrap(),
            client_order_id: client_order_id.to_string(),
            reduce_only: false,
        }
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

    #[tokio::test]
    async fn recent_track_pnl_records_include_trades_and_funding() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"[
                    {"commission":"0.07819010","commissionAsset":"USDT","id":698759,"orderId":25851813,"price":"7819.01","qty":"0.002","marginAsset":"USDT","realizedPnl":"-0.91539999","side":"SELL","symbol":"BTCUSDT","time":1569514978020},
                    {"commission":"0.00001000","commissionAsset":"BNB","id":698760,"orderId":25851814,"price":"7820.00","qty":"0.003","marginAsset":"USDT","realizedPnl":"1.25000000","side":"BUY","symbol":"BTCUSDT","time":1569514979020}
                ]"#,
            ),
            MockResponse::json(
                200,
                r#"[{"symbol":"BTCUSDT","incomeType":"FUNDING_FEE","income":"-0.37500000","asset":"USDT","time":1570608000000,"tranId":9689322392,"tradeId":""}]"#,
            ),
        ])
        .await;
        let connected = build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:19080");
        let required_window = PnlHistoryWindow::try_new(
            chrono::DateTime::from_timestamp_millis(1_569_514_978_000).unwrap(),
            chrono::DateTime::from_timestamp_millis(1_570_608_000_000).unwrap(),
        )
        .unwrap();

        let outcome = connected
            .account()
            .get_recent_track_pnl_records(
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                &required_window,
            )
            .await
            .unwrap();

        let RecentTrackPnlRecords::Available(batch) = outcome else {
            panic!("Binance recent PNL must be available");
        };
        assert!(batch.covers(&required_window));
        let records = batch.records;
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[0].source_key.as_deref(),
            Some("binance:order_trade_update:btcusdt:698759")
        );
        assert_eq!(records[0].trading_fee().unwrap().value(), 0.07819010);
        assert_eq!(records[0].trading_fee().unwrap().asset().as_str(), "USDT");
        assert_eq!(
            records[1].source_key.as_deref(),
            Some("binance:order_trade_update:btcusdt:698760")
        );
        assert_eq!(records[1].realized_pnl().unwrap().value(), 1.25);
        assert_eq!(records[1].trading_fee().unwrap().value(), 0.00001000);
        assert_eq!(records[1].trading_fee().unwrap().asset().as_str(), "BNB");
        assert_eq!(
            records[2].source_key.as_deref(),
            Some("binance:funding_fee:btcusdt:1570608000000")
        );
        assert_eq!(records[2].funding_amount().unwrap().value(), -0.375);

        let requests = server.requests().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[0].path.starts_with("/fapi/v1/userTrades?"));
        assert!(requests[1].path.starts_with("/fapi/v1/income?"));
        for request in requests {
            assert!(request.path.contains("startTime=1569514978000"));
            assert!(request.path.contains("endTime=1570608000000"));
            assert!(request.path.contains("limit=1000"));
        }
    }

    #[tokio::test]
    async fn recent_track_pnl_records_are_incomplete_at_the_exchange_page_limit() {
        let trades = serde_json::to_string(
            &(0..1_000)
                .map(|id| {
                    serde_json::json!({
                        "commission": "0.01",
                        "commissionAsset": "USDT",
                        "id": id,
                        "orderId": id,
                        "price": "65000",
                        "qty": "0.001",
                        "marginAsset": "USDT",
                        "realizedPnl": "0.1",
                        "side": "SELL",
                        "symbol": "BTCUSDT",
                        "time": 1_700_000_000_000_i64 + id,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, &trades),
            MockResponse::json(200, "[]"),
        ])
        .await;
        let connected = build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:19080");
        let required_window = PnlHistoryWindow::try_new(
            chrono::DateTime::from_timestamp_millis(1_699_999_000_000).unwrap(),
            chrono::DateTime::from_timestamp_millis(1_700_001_000_000).unwrap(),
        )
        .unwrap();

        let outcome = connected
            .account()
            .get_recent_track_pnl_records(
                &Instrument::new(Venue::Binance, "BTCUSDT"),
                &required_window,
            )
            .await
            .unwrap();

        let RecentTrackPnlRecords::Available(batch) = outcome else {
            panic!("Binance recent PNL must be available");
        };
        assert_eq!(batch.records.len(), 1_000);
        assert!(!batch.covers(&required_window));
    }

    #[test]
    fn exchange_ports_can_be_built_from_distinct_port_components() {
        let connected = ExchangePorts::new(
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

    #[test]
    fn rest_client_implements_execution_port_directly() {
        fn assert_execution_port<T: ExecutionPort>() {}

        assert_execution_port::<BinanceRestClient>();
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
                quantity: PositionQty::new(0.01).unwrap(),
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
                quantity: PositionQty::new(0.01).unwrap(),
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
                quantity: PositionQty::new(0.01).unwrap(),
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

        assert_eq!(error.kind(), ExecutionPortErrorKind::CancelOutcomeUnknown);
        assert!(error.to_string().contains("Unknown order sent."));
    }

    #[tokio::test]
    async fn submit_order_maps_insufficient_margin_to_execution_kind() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            400,
            r#"{"code":-2019,"msg":"Margin is insufficient."}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: Side::Buy,
                price: 65000.0,
                quantity: PositionQty::new(0.1).unwrap(),
                client_order_id: "client-margin".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::InsufficientMargin);
        assert!(error.to_string().contains("Margin is insufficient"));
    }

    #[tokio::test]
    async fn submit_order_keeps_definitive_internal_error_final_without_retrying() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            500,
            r#"{"code":-1000,"msg":"internal error"}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-server-error"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::Failed);
        assert_eq!(
            server.requests().await.len(),
            1,
            "one submit dispatch must produce at most one POST"
        );
    }

    #[tokio::test]
    async fn submit_order_keeps_service_unavailable_final_without_retrying() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            503,
            r#"{"code":-1000,"msg":"Service Unavailable."}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-service-unavailable"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::Failed);
        assert_eq!(
            server.requests().await.len(),
            1,
            "one submit dispatch must produce at most one POST"
        );
    }

    #[tokio::test]
    async fn submit_order_keeps_documented_internal_error_final_without_retrying() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            503,
            r#"{"code":-1000,"msg":"Internal error; unable to process your request. Please try again."}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-documented-internal-error"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::Failed);
        assert_eq!(server.requests().await.len(), 1);
    }

    #[tokio::test]
    async fn submit_order_maps_documented_timeout_503_to_unknown_without_retrying() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            503,
            r#"{"code":-1000,"msg":"Unknown error, please check your request or try again later."}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-documented-timeout"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::SubmitOutcomeUnknown);
        assert_eq!(server.requests().await.len(), 1);
    }

    #[tokio::test]
    async fn submit_order_keeps_system_overload_final_as_rate_limited() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            503,
            r#"{"code":-1008,"msg":"Request throttled by system-level protection. Reduce-only/close-position orders are exempt. Please try again."}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-system-overload"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::RateLimited);
        assert_eq!(server.requests().await.len(), 1);
    }

    #[tokio::test]
    async fn submit_order_does_not_follow_temporary_redirect() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::redirect(307, "/redirected-submit"),
            MockResponse::json(
                200,
                r#"{"orderId":12345,"clientOrderId":"client-redirect","executedQty":"0","status":"NEW"}"#,
            ),
        ])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let result = execution
            .submit_order(submit_request("client-redirect"))
            .await;

        assert_eq!(
            server.requests().await.len(),
            1,
            "one submit dispatch must not repeat POST through an HTTP redirect"
        );
        assert_eq!(
            result.unwrap_err().kind(),
            ExecutionPortErrorKind::Failed,
            "an unfollowed redirect is a definitive HTTP response"
        );
    }

    #[tokio::test]
    async fn submit_order_maps_request_timeout_to_unknown_without_retrying() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            408,
            r#"{"code":-1000,"msg":"request timeout"}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-request-timeout"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::SubmitOutcomeUnknown);
        assert_eq!(server.requests().await.len(), 1);
    }

    #[tokio::test]
    async fn submit_order_keeps_rate_limit_final_without_retrying() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            429,
            r#"{"code":-1003,"msg":"Too many requests"}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-rate-limited"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::RateLimited);
        assert_eq!(server.requests().await.len(), 1);
    }

    #[tokio::test]
    async fn submit_order_maps_malformed_success_response_to_unknown() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(200, "not-json")]).await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-malformed-response"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::SubmitOutcomeUnknown);
    }

    #[tokio::test]
    async fn submit_order_does_not_leak_cancel_only_error_kind() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            400,
            r#"{"code":-2011,"msg":"Unknown order sent."}"#,
        )])
        .await;
        let execution =
            build_test_connected_with_urls(server.base_url(), "ws://127.0.0.1:1").execution();

        let error = execution
            .submit_order(submit_request("client-cancel-code"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::Failed);
    }

    #[tokio::test]
    async fn submit_order_keeps_connect_failure_final() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_address = listener.local_addr().unwrap();
        drop(listener);
        let execution = build_test_connected_with_urls(
            format!("http://{unavailable_address}"),
            "ws://127.0.0.1:1",
        )
        .execution();

        let error = execution
            .submit_order(submit_request("client-connect-failure"))
            .await
            .unwrap_err();

        assert_eq!(error.kind(), ExecutionPortErrorKind::Failed);
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
        assert_eq!(position.qty.value(), 0.25);
        assert_eq!(position.avg_price, 65000.5);
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
        headers: Vec<(String, String)>,
    }

    impl MockResponse {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
                headers: Vec::new(),
            }
        }

        fn redirect(status: u16, location: &str) -> Self {
            Self {
                status,
                body: String::new(),
                headers: vec![("location".to_string(), location.to_string())],
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

                    let response_headers = response
                        .headers
                        .iter()
                        .map(|(name, value)| format!("{name}: {value}\r\n"))
                        .collect::<String>();
                    let reply = format!(
                        "HTTP/1.1 {} {}\r\n{}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response.status,
                        reason_phrase(response.status),
                        response_headers,
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
            307 => "Temporary Redirect",
            _ => "Unknown",
        }
    }
}
