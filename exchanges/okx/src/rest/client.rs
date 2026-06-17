use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{SecondsFormat, TimeZone, Utc};
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::form_urlencoded::Serializer;

use poise_core::track::{Instrument, Venue};
use poise_engine::ledger::TrackPnlRecord;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt,
    OrderRequest, OrderStatus, Position,
};

use crate::instrument::{OkxInstrumentMetadata, OkxInstrumentRegistry};
use crate::mapper::{
    account_summary_from_balance, available_balance_from_balance,
    open_order_from_snapshot_with_metadata, position_from_snapshot_with_metadata, side_to_okx,
    track_pnl_record_from_funding_bill_with_metadata,
    track_pnl_record_from_trade_fill_with_metadata,
};
use crate::rest::auth::sign_okx_payload;
use crate::rest::error::OkxRestError;
use crate::rest::models::{
    AccountConfigSnapshot, BalanceSnapshot, FundingBillSnapshot, InstrumentInfo, MarkPriceSnapshot,
    OkxEnvelope, OrderAck, PendingOrderSnapshot, PositionSnapshot, ServerTime, TradeFillSnapshot,
};
use crate::{Config, Credentials};

#[derive(Debug, Clone, Copy)]
enum AuthMode {
    None,
    Signed,
}

const MAX_DECIMAL_SCALE: u32 = 16;
const OKX_NET_POSITION_MODE: &str = "net_mode";
const RECENT_TRADE_FILL_LIMIT: usize = 100;
const RECENT_FUNDING_BILL_LIMIT: usize = 100;
const OKX_FUNDING_BILL_SUBTYPES: [&str; 2] = ["173", "174"];

pub(crate) struct OkxRestClient {
    http: reqwest::Client,
    base_url: String,
    credentials: Credentials,
    simulated_trading: bool,
    timestamp_provider: Arc<dyn Fn() -> chrono::DateTime<Utc> + Send + Sync>,
    instrument_registry: Arc<OkxInstrumentRegistry>,
}

impl OkxRestClient {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        Self::new_with_instrument_registry(config, Arc::new(OkxInstrumentRegistry::default()))
    }

    pub(crate) fn new_with_instrument_registry(
        config: &Config,
        instrument_registry: Arc<OkxInstrumentRegistry>,
    ) -> Result<Self> {
        let endpoints = config.endpoints();
        let base_url = endpoints.rest_base_url().to_string();
        Ok(Self {
            http: build_http_client(&base_url),
            base_url,
            credentials: config.credentials()?,
            simulated_trading: endpoints.simulated_trading(),
            timestamp_provider: Arc::new(Utc::now),
            instrument_registry,
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
            instrument_registry: Arc::new(OkxInstrumentRegistry::default()),
        }
    }

    pub(crate) async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo> {
        let instrument = self.fetch_instrument_info(symbol).await?;
        let metadata = OkxInstrumentMetadata::from_instrument_info(&instrument)?;
        let exchange_info = metadata.exchange_info_from_instrument(&instrument)?;
        self.instrument_registry.upsert(metadata);
        Ok(exchange_info)
    }

    pub(crate) async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        let balance = self.get_balance_snapshot().await?;
        account_summary_from_balance(balance)
    }

    pub(crate) async fn get_mark_price(&self, symbol: &str) -> Result<f64> {
        let response: Vec<MarkPriceSnapshot> = self
            .send_request(
                Method::GET,
                "/api/v5/public/mark-price",
                vec![
                    ("instType", "SWAP".to_string()),
                    ("instId", symbol.to_string()),
                ],
                None,
                AuthMode::None,
            )
            .await?;
        let snapshot = response
            .into_iter()
            .find(|snapshot| snapshot.inst_id == symbol)
            .with_context(|| format!("OKX mark price not found: {symbol}"))?;
        parse_decimal("markPx", &snapshot.mark_px)
    }

    pub(crate) async fn validate_net_position_mode(&self) -> Result<()> {
        let response: Vec<AccountConfigSnapshot> = self
            .send_request(
                Method::GET,
                "/api/v5/account/config",
                Vec::new(),
                None,
                AuthMode::Signed,
            )
            .await?;
        let config = response
            .into_iter()
            .next()
            .context("missing OKX account config")?;
        if config.pos_mode != OKX_NET_POSITION_MODE {
            return Err(anyhow!(
                "OKX account position mode must be `{}`, got `{}`; switch OKX to single-position/net mode before starting",
                OKX_NET_POSITION_MODE,
                config.pos_mode
            ));
        }
        Ok(())
    }

    pub(crate) async fn get_available_balance(&self, symbol: &str) -> Result<f64> {
        let metadata = self.get_or_fetch_instrument_metadata(symbol).await?;
        let balance = self.get_balance_snapshot().await?;
        available_balance_from_balance(&balance, metadata.settlement_asset())
    }

    pub(crate) async fn get_account_capacity_snapshot(
        &self,
        symbol: &str,
    ) -> Result<AccountCapacitySnapshot> {
        let metadata = self.get_or_fetch_instrument_metadata(symbol).await?;
        let summary = self.get_account_summary().await?;
        let position = self.get_position_snapshot(symbol).await?.ok_or_else(|| {
            anyhow!("OKX account capacity unavailable for `{symbol}`: position missing")
        })?;
        let leverage = parse_decimal("lever", &position.lever)?;
        if metadata.is_inverse() {
            let mark_price = self
                .mark_price_for_capacity(symbol, position.mark_px.as_deref())
                .await?;
            let contract_notional = metadata
                .contract_notional()
                .context("OKX inverse account capacity requires contract notional")?;
            let available = summary
                .available_for_asset(metadata.settlement_asset())
                .with_context(|| {
                    format!(
                        "missing OKX balance detail for settlement asset `{}`",
                        metadata.settlement_asset()
                    )
                })?;
            return Ok(AccountCapacitySnapshot {
                max_increase_notional: capacity_notional_from_inverse_available(
                    available,
                    mark_price,
                    leverage,
                    contract_notional,
                ),
            });
        }

        let available = summary
            .available_for_asset(metadata.settlement_asset())
            .with_context(|| {
                format!(
                    "missing OKX balance detail for settlement asset `{}`",
                    metadata.settlement_asset()
                )
            })?;
        Ok(AccountCapacitySnapshot {
            max_increase_notional: available * leverage,
        })
    }

    pub(crate) async fn get_position(&self, symbol: &str) -> Result<Position> {
        let metadata = self.get_or_fetch_instrument_metadata(symbol).await?;
        match self.get_position_snapshot(symbol).await? {
            Some(position) => position_from_snapshot_with_metadata(position, Some(&metadata)),
            None => Ok(Position {
                instrument: Instrument::new(Venue::Okx, symbol),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
                mark_price: None,
            }),
        }
    }

    pub(crate) async fn get_open_orders(&self, symbol: &str) -> Result<Vec<ExchangeOrder>> {
        let metadata = self.get_or_fetch_instrument_metadata(symbol).await?;
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
            .map(|order| open_order_from_snapshot_with_metadata(order, Some(&metadata)))
            .collect::<Result<Vec<_>>>()
    }

    pub(crate) async fn get_recent_track_pnl_records(
        &self,
        symbol: &str,
    ) -> Result<Vec<TrackPnlRecord>> {
        let metadata = self.get_or_fetch_instrument_metadata(symbol).await?;
        let mut records = self
            .get_recent_trade_pnl_records_with_metadata(symbol, &metadata)
            .await?;
        records.extend(
            self.get_recent_funding_pnl_records_with_metadata(symbol, &metadata)
                .await?,
        );
        Ok(records)
    }

    async fn get_recent_trade_pnl_records_with_metadata(
        &self,
        symbol: &str,
        metadata: &OkxInstrumentMetadata,
    ) -> Result<Vec<TrackPnlRecord>> {
        let response: Vec<TradeFillSnapshot> = self
            .send_request(
                Method::GET,
                "/api/v5/trade/fills",
                vec![
                    ("instType", "SWAP".to_string()),
                    ("instId", symbol.to_string()),
                    ("limit", RECENT_TRADE_FILL_LIMIT.to_string()),
                ],
                None,
                AuthMode::Signed,
            )
            .await?;
        response
            .into_iter()
            .map(|fill| track_pnl_record_from_trade_fill_with_metadata(fill, Some(metadata)))
            .collect::<Result<Vec<_>>>()
    }

    async fn get_recent_funding_pnl_records_with_metadata(
        &self,
        symbol: &str,
        metadata: &OkxInstrumentMetadata,
    ) -> Result<Vec<TrackPnlRecord>> {
        let mut records = Vec::new();
        for sub_type in OKX_FUNDING_BILL_SUBTYPES {
            let response: Vec<FundingBillSnapshot> = self
                .send_request(
                    Method::GET,
                    "/api/v5/account/bills",
                    vec![
                        ("instType", "SWAP".to_string()),
                        ("instId", symbol.to_string()),
                        ("subType", sub_type.to_string()),
                        ("limit", RECENT_FUNDING_BILL_LIMIT.to_string()),
                    ],
                    None,
                    AuthMode::Signed,
                )
                .await?;
            for bill in response {
                records.push(track_pnl_record_from_funding_bill_with_metadata(
                    bill,
                    Some(metadata),
                )?);
            }
        }
        Ok(records)
    }

    pub(crate) async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        let metadata = self
            .get_or_fetch_instrument_metadata(&req.instrument.symbol)
            .await?;
        let order_size = metadata.okx_contract_qty_from_native(req.quantity);
        let body = serde_json::to_string(&PlaceOrderBody {
            inst_id: req.instrument.symbol,
            td_mode: "cross",
            cl_ord_id: req.client_order_id,
            side: side_to_okx(req.side),
            pos_side: "net",
            ord_type: "limit",
            price: format_decimal(req.price),
            size: format_decimal(order_size),
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

    async fn mark_price_for_capacity(
        &self,
        symbol: &str,
        position_mark_price: Option<&str>,
    ) -> Result<f64> {
        match position_mark_price {
            Some(mark_price) => parse_decimal("markPx", mark_price),
            None => self.get_mark_price(symbol).await,
        }
    }

    async fn get_or_fetch_instrument_metadata(
        &self,
        symbol: &str,
    ) -> Result<OkxInstrumentMetadata> {
        if let Some(metadata) = self.instrument_registry.get(symbol) {
            return Ok(metadata);
        }

        let instrument = self.fetch_instrument_info(symbol).await?;
        let metadata = OkxInstrumentMetadata::from_instrument_info(&instrument)?;
        self.instrument_registry.upsert(metadata.clone());
        Ok(metadata)
    }

    async fn fetch_instrument_info(&self, symbol: &str) -> Result<InstrumentInfo> {
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
        response
            .into_iter()
            .find(|item| item.inst_id == symbol)
            .with_context(|| format!("OKX instrument not found: {symbol}"))
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
    #[serde(rename = "posSide")]
    pos_side: &'a str,
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

fn capacity_notional_from_inverse_available(
    available: f64,
    mark_price: f64,
    leverage: f64,
    contract_notional: f64,
) -> f64 {
    let estimated_contracts = available * mark_price * leverage / contract_notional;
    estimated_contracts * contract_notional
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
                linear_instrument_response(),
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
    async fn available_balance_uses_settlement_asset_from_metadata() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, linear_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"totalEq":"12500.5","details":[
                {"ccy":"USDT","availEq":"9800.25","upl":"-120.75"},
                {"ccy":"BTC","availEq":"200.0","upl":"10.0"}
            ]}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let available = client.get_available_balance("BTC-USDT-SWAP").await.unwrap();

        assert_eq!(available, 9_800.25);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].path,
            "/api/v5/public/instruments?instType=SWAP&instId=BTC-USDT-SWAP"
        );
        assert_eq!(requests[1].path, "/api/v5/account/balance");
    }

    #[tokio::test]
    async fn inverse_available_balance_uses_settlement_asset() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, inverse_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"totalEq":"12500.5","details":[
                {"ccy":"USD","availEq":"9800.25","upl":"0"},
                {"ccy":"BTC","availEq":"0.5","upl":"0"}
            ]}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let available = client.get_available_balance("BTC-USD-SWAP").await.unwrap();

        assert_eq!(available, 0.5);
    }

    #[tokio::test]
    async fn submit_order_posts_cross_limit_body() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, linear_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"ordId":"123","clOrdId":"client-1","sCode":"0","sMsg":""}]}"#,
            ),
        ])
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

        let requests = server.requests();
        assert_eq!(
            requests[0].path,
            "/api/v5/public/instruments?instType=SWAP&instId=BTC-USDT-SWAP"
        );
        let request = &requests[1];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/v5/trade/order");
        let body = request.json_body();
        assert_eq!(body["instId"], "BTC-USDT-SWAP");
        assert_eq!(body["tdMode"], "cross");
        assert_eq!(body["ordType"], "limit");
        assert_eq!(body["clOrdId"], "client-1");
        assert_eq!(body["side"], "buy");
        assert_eq!(body["posSide"], "net");
        assert_eq!(body["px"], "64000.1");
        assert_eq!(body["sz"], "1");
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
    async fn submit_inverse_order_keeps_contract_quantity() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, inverse_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"ordId":"123","clOrdId":"client-1","sCode":"0","sMsg":""}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        client
            .submit_order(OrderRequest {
                instrument: Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
                side: Side::Sell,
                price: 64000.10,
                quantity: 30.0,
                client_order_id: "client-1".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();

        let request = &server.requests()[1];
        let body = request.json_body();
        assert_eq!(body["instId"], "BTC-USD-SWAP");
        assert_eq!(body["posSide"], "net");
        assert_eq!(body["sz"], "30");
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
            MockResponse::json(200, linear_instrument_response()),
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
            "/api/v5/public/instruments?instType=SWAP&instId=BTC-USDT-SWAP"
        );
        assert_eq!(requests[1].method, "GET");
        assert_eq!(
            requests[1].path,
            "/api/v5/trade/orders-pending?instType=SWAP&instId=BTC-USDT-SWAP"
        );
        assert_eq!(requests[2].method, "POST");
        assert_eq!(requests[2].path, "/api/v5/trade/cancel-batch-orders");
        let body = requests[2].json_body();
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
            MockResponse::json(200, linear_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"totalEq":"12500.5","details":[
                    {"ccy":"USDT","availEq":"100.25","upl":"0"},
                    {"ccy":"BTC","availEq":"9.0","upl":"0"}
                ]}]}"#,
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
    async fn inverse_account_capacity_uses_settlement_asset_mark_price_and_leverage() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, inverse_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"totalEq":"50000","details":[{"ccy":"BTC","availEq":"0.5","upl":"0"}]}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USD-SWAP","pos":"0","avgPx":"0","markPx":"100000","upl":"0","posSide":"net","lever":"2"}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let snapshot = client
            .get_account_capacity_snapshot("BTC-USD-SWAP")
            .await
            .unwrap();

        assert_eq!(snapshot.max_increase_notional, 100_000.0);
    }

    #[tokio::test]
    async fn inverse_account_capacity_declines_with_mark_price() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, inverse_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"totalEq":"25000","details":[{"ccy":"BTC","availEq":"0.5","upl":"0"}]}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USD-SWAP","pos":"0","avgPx":"0","markPx":"50000","upl":"0","posSide":"net","lever":"2"}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let snapshot = client
            .get_account_capacity_snapshot("BTC-USD-SWAP")
            .await
            .unwrap();

        assert_eq!(snapshot.max_increase_notional, 50_000.0);
    }

    #[tokio::test]
    async fn inverse_account_capacity_fetches_public_mark_price_when_position_mark_missing() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, inverse_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"totalEq":"37500","details":[{"ccy":"BTC","availEq":"0.5","upl":"0"}]}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USD-SWAP","pos":"0","avgPx":"0","upl":"0","posSide":"net","lever":"2"}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USD-SWAP","instType":"SWAP","markPx":"75000","ts":"1781068207856"}]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let snapshot = client
            .get_account_capacity_snapshot("BTC-USD-SWAP")
            .await
            .unwrap();

        assert_eq!(snapshot.max_increase_notional, 75_000.0);
        assert_eq!(
            server.requests()[3].path,
            "/api/v5/public/mark-price?instType=SWAP&instId=BTC-USD-SWAP"
        );
    }

    #[tokio::test]
    async fn mark_price_reads_public_mark_price_endpoint() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"instId":"BTC-USD-SWAP","instType":"SWAP","markPx":"61123.7","ts":"1781068207856"}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let mark_price = client.get_mark_price("BTC-USD-SWAP").await.unwrap();

        assert_eq!(mark_price, 61_123.7);
        assert_eq!(
            server.requests()[0].path,
            "/api/v5/public/mark-price?instType=SWAP&instId=BTC-USD-SWAP"
        );
    }

    #[tokio::test]
    async fn recent_track_pnl_records_read_trade_fills() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, inverse_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[
                    {
                        "instId":"BTC-USD-SWAP",
                        "ordId":"3645412589617586176",
                        "tradeId":"460607985",
                        "side":"sell",
                        "fillPx":"62096.9",
                        "fillSz":"0.2",
                        "fillPnl":"0",
                        "fee":"-0.0000001610386348",
                        "feeCcy":"BTC",
                        "ts":"1781144161232"
                    }
                ]}"#,
            ),
            MockResponse::json(200, r#"{"code":"0","msg":"","data":[]}"#),
            MockResponse::json(200, r#"{"code":"0","msg":"","data":[]}"#),
        ])
        .await;
        let client = test_client(&server, true);

        let records = client
            .get_recent_track_pnl_records("BTC-USD-SWAP")
            .await
            .unwrap();

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(
            record.source_key.as_deref(),
            Some("okx:orders:btc-usd-swap:460607985")
        );
        assert_eq!(record.pnl_asset, "BTC");
        assert_eq!(record.qty, Some(0.2));
        assert_eq!(record.realized_pnl, 0.0);
        assert_eq!(record.trading_fee, 0.0000001610386348);

        let requests = server.requests();
        assert_eq!(
            requests[1].path,
            "/api/v5/trade/fills?instType=SWAP&instId=BTC-USD-SWAP&limit=100"
        );
        assert_eq!(
            requests[2].path,
            "/api/v5/account/bills?instType=SWAP&instId=BTC-USD-SWAP&subType=173&limit=100"
        );
        assert_eq!(
            requests[3].path,
            "/api/v5/account/bills?instType=SWAP&instId=BTC-USD-SWAP&subType=174&limit=100"
        );
        assert_eq!(
            requests[1].headers.get("ok-access-key"),
            Some(&"api-key".to_string())
        );
    }

    #[tokio::test]
    async fn recent_track_pnl_records_read_funding_bills() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, inverse_instrument_response()),
            MockResponse::json(200, r#"{"code":"0","msg":"","data":[]}"#),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[
                    {
                        "instId":"BTC-USD-SWAP",
                        "billId":"bill-expense",
                        "subType":"173",
                        "balChg":"-0.0001",
                        "ccy":"BTC",
                        "ts":"1781145600000"
                    }
                ]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[
                    {
                        "instId":"BTC-USD-SWAP",
                        "billId":"bill-income",
                        "subType":"174",
                        "balChg":"0.00025",
                        "ccy":"BTC",
                        "ts":"1781174400000"
                    }
                ]}"#,
            ),
        ])
        .await;
        let client = test_client(&server, true);

        let records = client
            .get_recent_track_pnl_records("BTC-USD-SWAP")
            .await
            .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].source_key.as_deref(),
            Some("okx:bills:btc-usd-swap:bill-expense")
        );
        assert_eq!(records[0].pnl_asset, "BTC");
        assert_eq!(records[0].funding_fee, -0.0001);
        assert_eq!(
            records[1].source_key.as_deref(),
            Some("okx:bills:btc-usd-swap:bill-income")
        );
        assert_eq!(records[1].funding_fee, 0.00025);
    }

    #[tokio::test]
    async fn validates_net_position_mode_from_account_config() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"posMode":"net_mode"}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        client.validate_net_position_mode().await.unwrap();

        assert_eq!(server.requests()[0].path, "/api/v5/account/config");
        assert_eq!(
            server.requests()[0].headers.get("ok-access-key"),
            Some(&"api-key".to_string())
        );
    }

    #[tokio::test]
    async fn rejects_long_short_position_mode_at_startup_validation() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"posMode":"long_short_mode"}]}"#,
        )])
        .await;
        let client = test_client(&server, true);

        let error = client.validate_net_position_mode().await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("net_mode"), "{message}");
        assert!(message.contains("long_short_mode"), "{message}");
        assert!(message.contains("single-position"), "{message}");
    }

    #[tokio::test]
    async fn maps_position_and_server_time_responses() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, linear_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","pos":"-25","avgPx":"65000.5","markPx":"65100.5","upl":"123.45","posSide":"net","lever":"20"}]}"#,
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
        assert_eq!(position.mark_price, Some(65100.5));
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
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, linear_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"51008","msg":"insufficient margin","data":[]}"#,
            ),
        ])
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
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, linear_instrument_response()),
            MockResponse::json(
                200,
                r#"{"code":"1","msg":"All operations failed","data":[{"ordId":"","clOrdId":"client-1","sCode":"51000","sMsg":"Parameter posSide error"}]}"#,
            ),
        ])
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

    fn linear_instrument_response() -> &'static str {
        r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","ctType":"linear","tickSz":"0.1","lotSz":"0.01","minSz":"0.01","ctVal":"0.01","ctValCcy":"BTC","settleCcy":"USDT"}]}"#
    }

    fn inverse_instrument_response() -> &'static str {
        r#"{"code":"0","msg":"","data":[{"instId":"BTC-USD-SWAP","ctType":"inverse","tickSz":"0.1","lotSz":"1","minSz":"1","ctVal":"100","ctValCcy":"USD","settleCcy":"BTC"}]}"#
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
