use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{SecondsFormat, TimeZone, Utc};
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::form_urlencoded::Serializer;

use poise_core::track::{Instrument, Venue};
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt,
    OrderRequest, OrderStatus, Position,
};

use crate::mapper::{
    account_summary_from_balance, available_balance_from_balance, exchange_info_from_instrument,
    open_order_from_snapshot, position_from_snapshot, side_to_okx,
};
use crate::rest::auth::sign_okx_payload;
use crate::rest::error::OkxRestError;
use crate::rest::models::{
    BalanceSnapshot, InstrumentInfo, OkxEnvelope, OrderAck, PendingOrderSnapshot, PositionSnapshot,
    ServerTime,
};
use crate::{Config, Credentials};

#[derive(Debug, Clone, Copy)]
enum AuthMode {
    None,
    Signed,
}

const MAX_DECIMAL_SCALE: u32 = 16;

pub(crate) struct OkxRestClient {
    http: reqwest::Client,
    base_url: String,
    credentials: Credentials,
    simulated_trading: bool,
    timestamp_provider: Arc<dyn Fn() -> chrono::DateTime<Utc> + Send + Sync>,
}

impl OkxRestClient {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        let endpoints = config.endpoints();
        let base_url = endpoints.rest_base_url().to_string();
        Ok(Self {
            http: build_http_client(&base_url),
            base_url,
            credentials: config.credentials()?,
            simulated_trading: endpoints.simulated_trading(),
            timestamp_provider: Arc::new(Utc::now),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_http_client_and_timestamp_provider(
        base_url: impl Into<String>,
        credentials: Credentials,
        simulated_trading: bool,
        timestamp_provider: Arc<dyn Fn() -> chrono::DateTime<Utc> + Send + Sync>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            credentials,
            simulated_trading,
            timestamp_provider,
        }
    }

    pub(crate) async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo> {
        let response: Vec<InstrumentInfo> = self
            .send_request(
                Method::GET,
                "/api/v5/public/instruments",
                vec![
                    ("instType", "SWAP".to_string()),
                    ("instId", symbol.to_string()),
                ],
                None,
                AuthMode::None,
            )
            .await?;
        let instrument = response
            .into_iter()
            .find(|item| item.inst_id == symbol)
            .with_context(|| format!("OKX instrument not found: {symbol}"))?;
        exchange_info_from_instrument(instrument)
    }

    pub(crate) async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        let balance = self.get_balance_snapshot().await?;
        account_summary_from_balance(balance)
    }

    pub(crate) async fn get_available_balance(&self, symbol: &str) -> Result<f64> {
        let quote_asset = Instrument::new(Venue::Okx, symbol).quote_asset();
        let balance = self.get_balance_snapshot().await?;
        available_balance_from_balance(&balance, &quote_asset)
    }

    pub(crate) async fn get_account_capacity_snapshot(
        &self,
        symbol: &str,
    ) -> Result<AccountCapacitySnapshot> {
        let summary = self.get_account_summary().await?;
        let position = self.get_position_snapshot(symbol).await?.ok_or_else(|| {
            anyhow!("OKX account capacity unavailable for `{symbol}`: position missing")
        })?;
        let leverage = parse_decimal("lever", &position.lever)?;
        Ok(AccountCapacitySnapshot {
            max_increase_notional: summary.available * leverage,
        })
    }

    pub(crate) async fn get_position(&self, symbol: &str) -> Result<Position> {
        match self.get_position_snapshot(symbol).await? {
            Some(position) => position_from_snapshot(position),
            None => Ok(Position {
                instrument: Instrument::new(Venue::Okx, symbol),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
            }),
        }
    }

    pub(crate) async fn get_open_orders(&self, symbol: &str) -> Result<Vec<ExchangeOrder>> {
        let response: Vec<PendingOrderSnapshot> = self
            .send_request(
                Method::GET,
                "/api/v5/trade/orders-pending",
                vec![
                    ("instType", "SWAP".to_string()),
                    ("instId", symbol.to_string()),
                ],
                None,
                AuthMode::Signed,
            )
            .await?;
        response
            .into_iter()
            .map(open_order_from_snapshot)
            .collect::<Result<Vec<_>>>()
    }

    pub(crate) async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        let body = serde_json::to_string(&PlaceOrderBody {
            inst_id: req.instrument.symbol,
            td_mode: "cross",
            cl_ord_id: req.client_order_id,
            side: side_to_okx(req.side),
            ord_type: "limit",
            price: format_decimal(req.price),
            size: format_decimal(req.quantity),
            reduce_only: req.reduce_only.then_some(true),
        })
        .context("failed to serialize OKX place-order body")?;
        let ack = self
            .send_ack_request(Method::POST, "/api/v5/trade/order", body)
            .await?;
        ack_to_receipt(ack, OrderStatus::Submitting)
    }

    pub(crate) async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<OrderReceipt> {
        let body = serde_json::to_string(&CancelOrderBody {
            inst_id: symbol.to_string(),
            order_id: order_id.to_string(),
        })
        .context("failed to serialize OKX cancel-order body")?;
        let ack = self
            .send_ack_request(Method::POST, "/api/v5/trade/cancel-order", body)
            .await?;
        ack_to_receipt(ack, OrderStatus::Canceled)
    }

    pub(crate) async fn cancel_all(&self, symbol: &str) -> Result<()> {
        let orders = self.get_open_orders(symbol).await?;
        if orders.is_empty() {
            return Ok(());
        }
        let body = serde_json::to_string(
            &orders
                .iter()
                .map(|order| CancelOrderBody {
                    inst_id: order.instrument.symbol.clone(),
                    order_id: order.order_id.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .context("failed to serialize OKX batch-cancel body")?;
        let acknowledgements: Vec<OrderAck> = self
            .send_request(
                Method::POST,
                "/api/v5/trade/cancel-batch-orders",
                Vec::new(),
                Some(body),
                AuthMode::Signed,
            )
            .await?;
        for ack in acknowledgements {
            ensure_ack_success(&ack)?;
        }
        Ok(())
    }

    pub(crate) async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        let body = serde_json::to_string(&SetLeverageBody {
            inst_id: symbol.to_string(),
            lever: leverage.to_string(),
            margin_mode: "cross",
        })
        .context("failed to serialize OKX set-leverage body")?;
        let _: Vec<serde_json::Value> = self
            .send_request(
                Method::POST,
                "/api/v5/account/set-leverage",
                Vec::new(),
                Some(body),
                AuthMode::Signed,
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        let response: Vec<ServerTime> = self
            .send_request(
                Method::GET,
                "/api/v5/public/time",
                Vec::new(),
                None,
                AuthMode::None,
            )
            .await?;
        let time = response
            .into_iter()
            .next()
            .context("missing OKX server time")?;
        let timestamp_ms = time
            .ts
            .parse::<i64>()
            .with_context(|| format!("invalid OKX server time: {}", time.ts))?;
        Utc.timestamp_millis_opt(timestamp_ms)
            .single()
            .ok_or_else(|| anyhow!("invalid OKX server timestamp: {timestamp_ms}"))
    }

    async fn get_balance_snapshot(&self) -> Result<BalanceSnapshot> {
        let response: Vec<BalanceSnapshot> = self
            .send_request(
                Method::GET,
                "/api/v5/account/balance",
                Vec::new(),
                None,
                AuthMode::Signed,
            )
            .await?;
        response
            .into_iter()
            .next()
            .context("missing OKX balance snapshot")
    }

    async fn get_position_snapshot(&self, symbol: &str) -> Result<Option<PositionSnapshot>> {
        let response: Vec<PositionSnapshot> = self
            .send_request(
                Method::GET,
                "/api/v5/account/positions",
                vec![
                    ("instType", "SWAP".to_string()),
                    ("instId", symbol.to_string()),
                ],
                None,
                AuthMode::Signed,
            )
            .await?;
        Ok(response
            .into_iter()
            .find(|position| position.inst_id == symbol))
    }

    async fn send_ack_request(&self, method: Method, path: &str, body: String) -> Result<OrderAck> {
        let acknowledgements: Vec<OrderAck> = self
            .send_request_allowing_ack_failure(method, path, body)
            .await?;
        let ack = acknowledgements
            .into_iter()
            .next()
            .with_context(|| format!("missing OKX acknowledgement for {path}"))?;
        ensure_ack_success(&ack)?;
        Ok(ack)
    }

    async fn send_request_allowing_ack_failure(
        &self,
        method: Method,
        path: &str,
        body: String,
    ) -> Result<Vec<OrderAck>> {
        let response_body = self
            .send_raw_request(
                method.clone(),
                path,
                Vec::new(),
                Some(body),
                AuthMode::Signed,
            )
            .await?;
        let envelope: OkxEnvelope<OrderAck> =
            serde_json::from_str(&response_body).with_context(|| {
                format!("failed to deserialize OKX response for {path}: {response_body}")
            })?;
        if envelope.code != "0" && envelope.data.is_empty() {
            return Err(
                OkxRestError::business_code(method, path, envelope.code, envelope.msg).into(),
            );
        }
        Ok(envelope.data)
    }

    async fn send_request<T>(
        &self,
        method: Method,
        path: &str,
        params: Vec<(&str, String)>,
        body: Option<String>,
        auth_mode: AuthMode,
    ) -> Result<Vec<T>>
    where
        T: DeserializeOwned,
    {
        let response_body = self
            .send_raw_request(method.clone(), path, params, body, auth_mode)
            .await?;
        let envelope: OkxEnvelope<T> = serde_json::from_str(&response_body).with_context(|| {
            format!("failed to deserialize OKX response for {path}: {response_body}")
        })?;
        if envelope.code != "0" {
            return Err(
                OkxRestError::business_code(method, path, envelope.code, envelope.msg).into(),
            );
        }
        Ok(envelope.data)
    }

    async fn send_raw_request(
        &self,
        method: Method,
        path: &str,
        params: Vec<(&str, String)>,
        body: Option<String>,
        auth_mode: AuthMode,
    ) -> Result<String> {
        let query = encode_query(&params);
        let request_path = if query.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{query}")
        };
        let url = format!("{}{}", self.base_url, request_path);
        let body_for_signing = body.as_deref().unwrap_or("");

        let mut request = self.http.request(method.clone(), &url);
        if let Some(body) = body.as_ref() {
            request = request
                .header("Content-Type", "application/json")
                .body(body.clone());
        }
        if matches!(auth_mode, AuthMode::Signed) {
            let timestamp = format_okx_timestamp((self.timestamp_provider)());
            let signature = sign_okx_payload(
                &timestamp,
                method.as_str(),
                &request_path,
                body_for_signing,
                self.credentials.api_secret(),
            );
            request = request
                .header("OK-ACCESS-KEY", self.credentials.api_key())
                .header("OK-ACCESS-SIGN", signature)
                .header("OK-ACCESS-TIMESTAMP", timestamp)
                .header("OK-ACCESS-PASSPHRASE", self.credentials.passphrase());
            if self.simulated_trading {
                request = request.header("x-simulated-trading", "1");
            }
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("request {} {} failed", method, path))?;
        let status = response.status();
        let response_body = response
            .text()
            .await
            .with_context(|| format!("failed to read OKX response body for {path}"))?;

        if !status.is_success() {
            return Err(OkxRestError::http_status(method, path, status, response_body).into());
        }
        Ok(response_body)
    }
}

#[derive(Serialize)]
struct PlaceOrderBody<'a> {
    #[serde(rename = "instId")]
    inst_id: String,
    #[serde(rename = "tdMode")]
    td_mode: &'a str,
    #[serde(rename = "clOrdId")]
    cl_ord_id: String,
    side: &'a str,
    #[serde(rename = "ordType")]
    ord_type: &'a str,
    #[serde(rename = "px")]
    price: String,
    #[serde(rename = "sz")]
    size: String,
    #[serde(rename = "reduceOnly", skip_serializing_if = "Option::is_none")]
    reduce_only: Option<bool>,
}

#[derive(Serialize)]
struct CancelOrderBody {
    #[serde(rename = "instId")]
    inst_id: String,
    #[serde(rename = "ordId")]
    order_id: String,
}

#[derive(Serialize)]
struct SetLeverageBody<'a> {
    #[serde(rename = "instId")]
    inst_id: String,
    lever: String,
    #[serde(rename = "mgnMode")]
    margin_mode: &'a str,
}

fn ack_to_receipt(ack: OrderAck, status: OrderStatus) -> Result<OrderReceipt> {
    ensure_ack_success(&ack)?;
    Ok(OrderReceipt {
        order_id: ack.order_id,
        client_order_id: ack.client_order_id,
        filled_qty: 0.0,
        status,
    })
}

fn ensure_ack_success(ack: &OrderAck) -> Result<()> {
    if ack.s_code != "0" {
        return Err(OkxRestError::acknowledgement(
            ack.order_id.clone(),
            ack.s_code.clone(),
            ack.s_msg.clone(),
        )
        .into());
    }
    Ok(())
}

fn encode_query(params: &[(&str, String)]) -> String {
    let mut serializer = Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn build_http_client(_base_url: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("failed to build OKX reqwest client")
}

fn format_okx_timestamp(timestamp: chrono::DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

fn format_decimal(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }

    for scale in 0..=MAX_DECIMAL_SCALE {
        let factor = 10_f64.powi(scale as i32);
        let scaled = value * factor;
        let rounded = scaled.round();
        let tolerance = scaled.abs().max(1.0) * f64::EPSILON * 16.0;
        if (scaled - rounded).abs() <= tolerance {
            let normalized = rounded / factor;
            return trim_decimal_string(format!("{normalized:.scale$}", scale = scale as usize));
        }
    }

    value.to_string()
}

fn trim_decimal_string(mut value: String) -> String {
    if value.contains('.') {
        while value.ends_with('0') {
            value.pop();
        }
        if value.ends_with('.') {
            value.pop();
        }
    }

    if value == "-0" {
        "0".to_string()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use chrono::{DateTime, Utc};
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::Side;
    use poise_engine::ports::{OrderRequest, OrderStatus};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::Config;
    use crate::rest::auth::sign_okx_payload;

    #[test]
    fn new_builds_client_from_config_without_network() {
        let config = Config {
            deployment: crate::Deployment::Demo,
            api_key: Some("api-key".to_string()),
            api_secret: Some("secret-key".to_string()),
            passphrase: Some("passphrase".to_string()),
        };

        let _client = OkxRestClient::new(&config).unwrap();
    }

    #[tokio::test]
    async fn requests_use_okx_paths_auth_headers_and_demo_header() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","tickSz":"0.1","lotSz":"0.01","minSz":"0.01"}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"totalEq":"12500.5","details":[{"ccy":"USDT","availEq":"9800.25","upl":"-120.75"}]}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let _ = client.get_exchange_info("BTC-USDT-SWAP").await.unwrap();
        let _ = client.get_account_summary().await.unwrap();

        let requests = server.requests();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].path,
            "/api/v5/public/instruments?instType=SWAP&instId=BTC-USDT-SWAP"
        );
        assert!(!requests[0].headers.contains_key("ok-access-key"));

        assert_eq!(requests[1].method, "GET");
        assert_eq!(requests[1].path, "/api/v5/account/balance");
        assert_eq!(
            requests[1].headers.get("ok-access-key"),
            Some(&"api-key".to_string())
        );
        assert_eq!(
            requests[1].headers.get("ok-access-passphrase"),
            Some(&"passphrase".to_string())
        );
        assert_eq!(
            requests[1].headers.get("ok-access-timestamp"),
            Some(&fixed_timestamp())
        );
        assert_eq!(
            requests[1].headers.get("ok-access-sign"),
            Some(&sign_okx_payload(
                &fixed_timestamp(),
                "GET",
                "/api/v5/account/balance",
                "",
                "secret-key",
            ))
        );
        assert_eq!(
            requests[1].headers.get("x-simulated-trading"),
            Some(&"1".to_string())
        );
    }

    #[tokio::test]
    async fn available_balance_uses_quote_asset_from_swap_symbol() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"totalEq":"12500.5","details":[
                {"ccy":"USDT","availEq":"9800.25","upl":"-120.75"},
                {"ccy":"BTC","availEq":"200.0","upl":"10.0"}
            ]}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let available = client.get_available_balance("BTC-USDT-SWAP").await.unwrap();

        assert_eq!(available, 9_800.25);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/api/v5/account/balance");
    }

    #[tokio::test]
    async fn submit_order_posts_cross_limit_body() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"ordId":"123","clOrdId":"client-1","sCode":"0","sMsg":""}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let receipt = client
            .submit_order(OrderRequest {
                instrument: Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
                side: Side::Buy,
                price: 64000.10,
                quantity: 0.01,
                client_order_id: "client-1".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();

        assert_eq!(receipt.order_id, "123");
        assert_eq!(receipt.client_order_id, "client-1");
        assert_eq!(receipt.status, OrderStatus::Submitting);

        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v5/trade/order");
        let body = request.json_body();
        assert_eq!(body["instId"], "BTC-USDT-SWAP");
        assert_eq!(body["tdMode"], "cross");
        assert_eq!(body["ordType"], "limit");
        assert_eq!(body["clOrdId"], "client-1");
        assert_eq!(body["side"], "buy");
        assert_eq!(body["px"], "64000.1");
        assert_eq!(body["sz"], "0.01");
        assert_eq!(
            request.headers.get("ok-access-sign"),
            Some(&sign_okx_payload(
                &fixed_timestamp(),
                "POST",
                "/api/v5/trade/order",
                &request.body,
                "secret-key",
            ))
        );
    }

    #[tokio::test]
    async fn cancel_order_posts_cancel_order_body() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"ordId":"123","clOrdId":"client-1","sCode":"0","sMsg":""}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let receipt = client.cancel_order("BTC-USDT-SWAP", "123").await.unwrap();

        assert_eq!(receipt.status, OrderStatus::Canceled);
        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v5/trade/cancel-order");
        let body = request.json_body();
        assert_eq!(body["instId"], "BTC-USDT-SWAP");
        assert_eq!(body["ordId"], "123");
    }

    #[tokio::test]
    async fn cancel_all_queries_pending_orders_then_posts_batch_cancel() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[
                    {"instId":"BTC-USDT-SWAP","ordId":"123","clOrdId":"client-1","side":"buy","px":"64000.1","sz":"0.01","accFillSz":"0","state":"live"},
                    {"instId":"BTC-USDT-SWAP","ordId":"456","clOrdId":"client-2","side":"sell","px":"65000.1","sz":"0.02","accFillSz":"0","state":"partially_filled"}
                ]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[
                    {"ordId":"123","clOrdId":"client-1","sCode":"0","sMsg":""},
                    {"ordId":"456","clOrdId":"client-2","sCode":"0","sMsg":""}
                ]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        client.cancel_all("BTC-USDT-SWAP").await.unwrap();

        let requests = server.requests();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].path,
            "/api/v5/trade/orders-pending?instType=SWAP&instId=BTC-USDT-SWAP"
        );
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/api/v5/trade/cancel-batch-orders");
        let body = requests[1].json_body();
        assert_eq!(body[0]["instId"], "BTC-USDT-SWAP");
        assert_eq!(body[0]["ordId"], "123");
        assert_eq!(body[1]["ordId"], "456");
    }

    #[tokio::test]
    async fn set_leverage_posts_cross_margin_body() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","lever":"10","mgnMode":"cross"}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        client.set_leverage("BTC-USDT-SWAP", 10).await.unwrap();

        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v5/account/set-leverage");
        let body = request.json_body();
        assert_eq!(body["instId"], "BTC-USDT-SWAP");
        assert_eq!(body["lever"], "10");
        assert_eq!(body["mgnMode"], "cross");
    }

    #[tokio::test]
    async fn account_capacity_scales_available_balance_by_position_leverage() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"totalEq":"12500.5","details":[{"ccy":"USDT","availEq":"100.25","upl":"0"}]}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","pos":"0","avgPx":"0","upl":"0","posSide":"net","lever":"10"}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let snapshot = client
            .get_account_capacity_snapshot("BTC-USDT-SWAP")
            .await
            .unwrap();

        assert_eq!(snapshot.max_increase_notional, 1002.5);
    }

    #[tokio::test]
    async fn maps_position_and_server_time_responses() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","pos":"-0.25","avgPx":"65000.5","upl":"123.45","posSide":"net","lever":"20"}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"ts":"1704876947123"}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let position = client.get_position("BTC-USDT-SWAP").await.unwrap();
        let server_time = client.get_server_time().await.unwrap();

        assert_eq!(position.qty, -0.25);
        assert_eq!(position.avg_price, 65000.5);
        assert_eq!(server_time.timestamp_millis(), 1_704_876_947_123);
    }

    #[tokio::test]
    async fn non_zero_okx_envelope_code_returns_path_error() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"51000","msg":"bad request","data":[]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let error = client.get_account_summary().await.unwrap_err().to_string();

        assert!(error.contains("GET /api/v5/account/balance"));
        assert!(error.contains("51000"));
        assert!(error.contains("bad request"));
    }

    #[tokio::test]
    async fn execution_port_maps_insufficient_margin_code_to_execution_kind() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"51008","msg":"insufficient margin","data":[]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let error = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            OrderRequest {
                instrument: Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
                side: Side::Buy,
                price: 64000.10,
                quantity: 0.01,
                client_order_id: "client-1".to_string(),
                reduce_only: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::InsufficientMargin
        );
        assert!(error.to_string().contains("51008"));
    }

    #[tokio::test]
    async fn submit_order_surfaces_ack_failure_when_envelope_reports_all_operations_failed() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"1","msg":"All operations failed","data":[{"ordId":"","clOrdId":"client-1","sCode":"51000","sMsg":"Parameter posSide error"}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let error = client
            .submit_order(OrderRequest {
                instrument: Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
                side: Side::Buy,
                price: 64000.10,
                quantity: 0.01,
                client_order_id: "client-1".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("sCode 51000"), "{message}");
        assert!(message.contains("Parameter posSide error"), "{message}");
    }

    #[tokio::test]
    async fn execution_port_maps_okx_cancel_race_to_cancel_outcome_unknown() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"ordId":"123","clOrdId":"client-1","sCode":"51400","sMsg":"Order cancellation failed as the order has been filled, canceled or does not exist"}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let error = poise_engine::ports::ExecutionPort::cancel_order(
            &client,
            &Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
            "123",
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::CancelOutcomeUnknown
        );
        assert!(error.to_string().contains("51400"));
    }

    fn test_client(server: &MockHttpServer, simulated_trading: bool) -> OkxRestClient {
        let config = Config {
            deployment: crate::Deployment::Demo,
            api_key: Some("api-key".to_string()),
            api_secret: Some("secret-key".to_string()),
            passphrase: Some("passphrase".to_string()),
        };
        OkxRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            config.credentials().unwrap(),
            simulated_trading,
            Arc::new(fixed_datetime),
            build_http_client(&server.base_url()),
        )
    }

    fn fixed_datetime() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2020-12-08T09:08:57.715Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn fixed_timestamp() -> String {
        "2020-12-08T09:08:57.715Z".to_string()
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
                    let request = parse_request(&String::from_utf8_lossy(&buffer));
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: String,
    }

    impl RecordedRequest {
        fn json_body(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).unwrap()
        }
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
