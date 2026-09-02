use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::{Mutex as AsyncMutex, OnceCell};

use poise_core::types::PriceRounding;
use poise_engine::ledger::TrackPnlRecord;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt,
    OrderRequest as PortOrderRequest, OrderStatus, Position,
};

use crate::client_order_id::ClientOrderIdMapper;
use crate::config::{Config, Credentials};
use crate::mapper::{
    account_summary_from_state, build_exchange_info, open_order_from_response, position_from_state,
    track_pnl_record_from_fill, track_pnl_record_from_funding,
};
use crate::rest::actions::{
    CancelAction, CancelRequest, ExchangeAction, LimitOrderType, OrderAction, OrderRequest,
    OrderType, UpdateLeverageAction,
};
use crate::rest::error::HyperliquidRestError;
use crate::rest::models::{
    ClearinghouseStateResponse, MetaResponse, OpenOrderResponse, PerpAssetMeta, PerpDexMeta,
    SpotClearinghouseStateResponse, UserFillResponse, UserFundingResponse,
};
use crate::rules::normalize_perp_price;
use crate::signing::{HyperliquidChain, action_hash, sign_l1_action};

const MAX_DECIMAL_SCALE: u32 = 16;
const RECENT_PNL_WINDOW_MILLIS: i64 = 24 * 60 * 60 * 1_000;
const USER_FILLS_PAGE_SIZE: usize = 2_000;
const USER_FILLS_RETENTION_LIMIT: usize = 10_000;
const USER_FUNDING_PAGE_SIZE: usize = 500;

pub(crate) struct HyperliquidRestClient {
    http: reqwest::Client,
    base_url: String,
    credentials: Credentials,
    timestamp_provider: Arc<dyn Fn() -> u64 + Send + Sync>,
    chain: HyperliquidChain,
    meta_cache: AsyncMutex<HashMap<PerpDexKey, MetaResponse>>,
    perp_dex_index_cache: AsyncMutex<HashMap<String, u32>>,
    uses_spot_margin_balance_cache: OnceCell<bool>,
    client_order_ids: Arc<ClientOrderIdMapper>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerpDexRef<'a> {
    Default,
    Hip3 { dex: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PerpDexKey {
    Default,
    Hip3(String),
}

impl PerpDexKey {
    fn from_ref(dex_ref: PerpDexRef<'_>) -> Self {
        match dex_ref {
            PerpDexRef::Default => Self::Default,
            PerpDexRef::Hip3 { dex } => Self::Hip3(dex.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AssetDescriptor {
    id: u32,
    sz_decimals: u32,
    max_leverage: Option<u32>,
    leverage_is_cross: bool,
}

impl HyperliquidRestClient {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        Self::new_with_client_order_id_mapper(config, ClientOrderIdMapper::shared())
    }

    pub(crate) fn new_with_client_order_id_mapper(
        config: &Config,
        client_order_ids: Arc<ClientOrderIdMapper>,
    ) -> Result<Self> {
        Ok(
            Self::with_http_client_timestamp_provider_and_client_order_id_mapper(
                config.endpoints().rest_base_url().to_string(),
                config.credentials()?,
                Arc::new(|| chrono::Utc::now().timestamp_millis() as u64),
                build_http_client()?,
                client_order_ids,
            ),
        )
    }

    #[cfg(test)]
    pub(crate) fn with_http_client_and_timestamp_provider(
        base_url: impl Into<String>,
        credentials: Credentials,
        timestamp_provider: Arc<dyn Fn() -> u64 + Send + Sync>,
        http: reqwest::Client,
    ) -> Self {
        Self::with_http_client_timestamp_provider_and_client_order_id_mapper(
            base_url,
            credentials,
            timestamp_provider,
            http,
            ClientOrderIdMapper::shared(),
        )
    }

    pub(crate) fn with_http_client_timestamp_provider_and_client_order_id_mapper(
        base_url: impl Into<String>,
        credentials: Credentials,
        timestamp_provider: Arc<dyn Fn() -> u64 + Send + Sync>,
        http: reqwest::Client,
        client_order_ids: Arc<ClientOrderIdMapper>,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let chain =
            if base_url.contains("hyperliquid.xyz") && !base_url.contains("hyperliquid-testnet") {
                HyperliquidChain::Mainnet
            } else {
                HyperliquidChain::Testnet
            };
        Self {
            http,
            base_url,
            credentials,
            timestamp_provider,
            chain,
            meta_cache: AsyncMutex::new(HashMap::new()),
            perp_dex_index_cache: AsyncMutex::new(HashMap::new()),
            uses_spot_margin_balance_cache: OnceCell::new(),
            client_order_ids,
        }
    }

    pub(crate) async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo> {
        let dex_ref = parse_perp_dex(symbol)?;
        let meta = self.meta(dex_ref).await?;
        build_exchange_info(&meta, symbol)
    }

    pub(crate) async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        let uses_spot_margin_balance = self.uses_spot_margin_balance().await?;
        let state = self.account_state().await?;
        let summary = account_summary_from_state(&state)?;
        if uses_spot_margin_balance {
            let spot_state = self.spot_user_state().await?;
            let asset =
                poise_core::instrument::Asset::try_new("USDC").map_err(anyhow::Error::msg)?;
            return summary
                .with_equity_and_available(
                    poise_core::instrument::Amount::new(
                        usdc_total_balance(&spot_state)?,
                        asset.clone(),
                    )
                    .map_err(anyhow::Error::msg)?,
                    poise_core::instrument::Amount::new(
                        usdc_available_after_maintenance(&spot_state)?,
                        asset,
                    )
                    .map_err(anyhow::Error::msg)?,
                )
                .map_err(anyhow::Error::msg);
        }
        Ok(summary)
    }

    pub(crate) async fn get_account_capacity_snapshot(
        &self,
        leverage: u32,
    ) -> Result<AccountCapacitySnapshot> {
        if self.uses_spot_margin_balance().await? {
            let spot_state = self.spot_user_state().await?;
            return Ok(AccountCapacitySnapshot {
                max_increase_notional: usdc_available_after_maintenance(&spot_state)?
                    * leverage as f64,
            });
        }

        let state = self.account_state().await?;
        let withdrawable = state
            .withdrawable
            .parse::<f64>()
            .context("invalid Hyperliquid withdrawable")?;
        Ok(AccountCapacitySnapshot {
            max_increase_notional: withdrawable * leverage as f64,
        })
    }

    pub(crate) async fn get_position(&self, symbol: &str) -> Result<Position> {
        let state = self.user_state_for_symbol(symbol).await?;
        position_from_state(&state, symbol)
    }

    pub(crate) async fn get_open_orders(&self, symbol: &str) -> Result<Vec<ExchangeOrder>> {
        let dex_ref = parse_perp_dex(symbol)?;
        let body = with_perp_dex(
            json!({
                "type": "openOrders",
                "user": self.credentials.account_address(),
            }),
            dex_ref,
        );
        let orders: Vec<OpenOrderResponse> = self.post_info(&body).await?;
        orders
            .into_iter()
            .filter(|order| order.coin == symbol)
            .map(|order| {
                let mut order = open_order_from_response(order)?;
                order.client_order_id = self
                    .client_order_ids
                    .local_id_for_exchange(&order.client_order_id);
                Ok(order)
            })
            .collect()
    }

    pub(crate) async fn get_recent_track_pnl_records(
        &self,
        symbol: &str,
    ) -> Result<Vec<TrackPnlRecord>> {
        let end_time = i64::try_from((self.timestamp_provider)())
            .context("Hyperliquid timestamp does not fit signed milliseconds")?;
        let start_time = end_time.saturating_sub(RECENT_PNL_WINDOW_MILLIS);
        let mut records = self
            .get_recent_fill_pnl_records(symbol, start_time, end_time)
            .await?;
        records.extend(
            self.get_recent_funding_pnl_records(symbol, start_time, end_time)
                .await?,
        );
        records.sort_by(|left, right| {
            left.occurred_at
                .cmp(&right.occurred_at)
                .then_with(|| left.source_key.cmp(&right.source_key))
        });
        Ok(records)
    }

    async fn get_recent_fill_pnl_records(
        &self,
        symbol: &str,
        start_time: i64,
        end_time: i64,
    ) -> Result<Vec<TrackPnlRecord>> {
        let mut cursor = start_time;
        let mut fills_by_key = HashMap::<(String, u64), UserFillResponse>::new();
        loop {
            let response: Vec<UserFillResponse> = self
                .post_info(&json!({
                    "type": "userFillsByTime",
                    "user": self.credentials.account_address(),
                    "startTime": cursor,
                    "endTime": end_time,
                    "aggregateByTime": false,
                }))
                .await?;
            validate_recent_history_times(
                "userFillsByTime",
                cursor,
                end_time,
                response.iter().map(|value| value.time),
            )?;
            let item_count = response.len();
            let latest_time = response.last().map(|value| value.time);
            let mut new_fill_keys = 0_usize;
            for fill in response {
                let fill_key = (fill.coin.clone(), fill.tid);
                match fills_by_key.get(&fill_key) {
                    None => {
                        new_fill_keys += 1;
                        fills_by_key.insert(fill_key, fill);
                    }
                    Some(existing) if existing == &fill => {}
                    Some(_) => {
                        return Err(anyhow!(
                            "conflicting Hyperliquid fill identity {}:{} across recent history pages",
                            fill_key.0,
                            fill_key.1
                        ));
                    }
                }
            }
            ensure_recent_fill_history_complete(fills_by_key.len())?;
            let Some(next_start) =
                next_recent_history_start(cursor, item_count, latest_time, USER_FILLS_PAGE_SIZE)?
            else {
                break;
            };
            ensure!(
                new_fill_keys > 0,
                "full userFillsByTime page contained no new fill identity; pagination made no progress"
            );
            cursor = next_start;
        }

        fills_by_key
            .values()
            .filter(|fill| fill.coin == symbol)
            .map(track_pnl_record_from_fill)
            .collect()
    }

    async fn get_recent_funding_pnl_records(
        &self,
        symbol: &str,
        start_time: i64,
        end_time: i64,
    ) -> Result<Vec<TrackPnlRecord>> {
        let mut cursor = start_time;
        let mut funding_by_key = HashMap::<(String, i64), UserFundingResponse>::new();
        loop {
            let response: Vec<UserFundingResponse> = self
                .post_info(&json!({
                    "type": "userFunding",
                    "user": self.credentials.account_address(),
                    "startTime": cursor,
                    "endTime": end_time,
                }))
                .await?;
            validate_recent_history_times(
                "userFunding",
                cursor,
                end_time,
                response.iter().map(|value| value.time),
            )?;
            let item_count = response.len();
            let latest_time = response.last().map(|value| value.time);
            let mut new_funding_keys = 0_usize;
            for funding in response {
                let hash = funding.hash.trim();
                ensure!(!hash.is_empty(), "missing Hyperliquid `funding.hash`");
                let funding_key = (funding.delta.coin.clone(), funding.time);
                match funding_by_key.get(&funding_key) {
                    None => {
                        new_funding_keys += 1;
                        funding_by_key.insert(funding_key, funding);
                    }
                    Some(existing) if existing == &funding => {}
                    Some(_) => {
                        return Err(anyhow!(
                            "conflicting Hyperliquid funding canonical key {}:{} across recent history pages",
                            funding_key.0,
                            funding_key.1
                        ));
                    }
                }
            }
            let Some(next_start) =
                next_recent_history_start(cursor, item_count, latest_time, USER_FUNDING_PAGE_SIZE)?
            else {
                break;
            };
            ensure!(
                new_funding_keys > 0,
                "full userFunding page contained no new funding identity; pagination made no progress"
            );
            cursor = next_start;
        }

        funding_by_key
            .values()
            .filter(|funding| funding.delta.coin == symbol)
            .map(track_pnl_record_from_funding)
            .collect()
    }

    pub(crate) async fn submit_order(&self, request: PortOrderRequest) -> Result<OrderReceipt> {
        let asset = self.asset_descriptor(&request.instrument.symbol).await?;
        let cloid = self
            .client_order_ids
            .exchange_id_for_local(&request.client_order_id);
        let action = ExchangeAction::Order(OrderAction {
            orders: vec![OrderRequest {
                asset: asset.id,
                is_buy: matches!(request.side, poise_core::types::Side::Buy),
                limit_px: format_price_decimal(request.price, asset.sz_decimals),
                sz: format_size_decimal(request.quantity, asset.sz_decimals),
                reduce_only: request.reduce_only,
                order_type: OrderType::Limit(LimitOrderType {
                    tif: "Gtc".to_string(),
                }),
                cloid: Some(cloid),
            }],
            grouping: "na".to_string(),
            builder: None,
        });
        let body = self.exchange_request_body(&action)?;
        let response = self
            .post_json("/exchange", &body)
            .await
            .map_err(HyperliquidRestError::classify_submit_dispatch)?;
        order_receipt_from_response(response, &request.client_order_id)
            .map_err(HyperliquidRestError::classify_submit_dispatch)
    }

    pub(crate) async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<OrderReceipt> {
        let asset = self.asset_id(symbol).await?;
        let oid = order_id
            .parse::<u64>()
            .with_context(|| format!("invalid Hyperliquid order id `{order_id}`"))?;
        let action = ExchangeAction::Cancel(CancelAction {
            cancels: vec![CancelRequest { asset, oid }],
        });
        let response = self.post_exchange(&action).await?;
        ensure_exchange_ok(response)?;
        Ok(OrderReceipt {
            order_id: order_id.to_string(),
            client_order_id: order_id.to_string(),
            filled_qty: 0.0,
            status: OrderStatus::Canceled,
        })
    }

    pub(crate) async fn cancel_all(&self, symbol: &str) -> Result<()> {
        let open_orders = self.get_open_orders(symbol).await?;
        if open_orders.is_empty() {
            return Ok(());
        }
        let asset = self.asset_id(symbol).await?;
        let cancels = open_orders
            .iter()
            .map(|order| {
                let oid = order.order_id.parse::<u64>().with_context(|| {
                    format!("invalid Hyperliquid order id `{}`", order.order_id)
                })?;
                Ok(CancelRequest { asset, oid })
            })
            .collect::<Result<Vec<_>>>()?;
        let action = ExchangeAction::Cancel(CancelAction { cancels });
        let response = self.post_exchange(&action).await?;
        ensure_exchange_ok(response)
    }

    pub(crate) async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        let asset = self.asset_descriptor(symbol).await?;
        if let Some(max_leverage) = asset.max_leverage {
            if leverage > max_leverage {
                return Err(anyhow!(
                    "Hyperliquid leverage {}x exceeds maxLeverage {}x for asset `{symbol}`",
                    leverage,
                    max_leverage
                ));
            }
        }
        let action = ExchangeAction::UpdateLeverage(UpdateLeverageAction {
            asset: asset.id,
            is_cross: asset.leverage_is_cross,
            leverage,
        });
        let response = self.post_exchange(&action).await?;
        ensure_exchange_ok(response)
    }

    async fn meta(&self, dex_ref: PerpDexRef<'_>) -> Result<MetaResponse> {
        let key = PerpDexKey::from_ref(dex_ref);
        if let Some(meta) = self.meta_cache.lock().await.get(&key).cloned() {
            return Ok(meta);
        }

        let body = with_perp_dex(json!({ "type": "meta" }), dex_ref);
        let meta: MetaResponse = self.post_info(&body).await?;
        self.meta_cache.lock().await.insert(key, meta.clone());
        Ok(meta)
    }

    async fn account_state(&self) -> Result<ClearinghouseStateResponse> {
        self.post_info(&json!({
            "type": "clearinghouseState",
            "user": self.credentials.account_address(),
        }))
        .await
    }

    async fn user_state_for_symbol(&self, symbol: &str) -> Result<ClearinghouseStateResponse> {
        let dex_ref = parse_perp_dex(symbol)?;
        let body = with_perp_dex(
            json!({
                "type": "clearinghouseState",
                "user": self.credentials.account_address(),
            }),
            dex_ref,
        );
        self.post_info(&body).await
    }

    async fn spot_user_state(&self) -> Result<SpotClearinghouseStateResponse> {
        self.post_info(&json!({
            "type": "spotClearinghouseState",
            "user": self.credentials.account_address(),
        }))
        .await
    }

    async fn uses_spot_margin_balance(&self) -> Result<bool> {
        self.uses_spot_margin_balance_cache
            .get_or_try_init(|| async {
                let abstraction: String = self
                    .post_info(&json!({
                        "type": "userAbstraction",
                        "user": self.credentials.account_address(),
                    }))
                    .await?;
                Ok(matches!(
                    abstraction.as_str(),
                    "unifiedAccount" | "portfolioMargin"
                ))
            })
            .await
            .copied()
    }

    async fn asset_id(&self, symbol: &str) -> Result<u32> {
        Ok(self.asset_descriptor(symbol).await?.id)
    }

    async fn asset_descriptor(&self, symbol: &str) -> Result<AssetDescriptor> {
        let dex_ref = parse_perp_dex(symbol)?;
        let meta = self.meta(dex_ref).await?;
        let (index, asset) = meta
            .universe
            .iter()
            .enumerate()
            .find(|(_, asset)| asset.name == symbol)
            .ok_or_else(|| match dex_ref {
                PerpDexRef::Default => anyhow!("missing Hyperliquid asset `{symbol}`"),
                PerpDexRef::Hip3 { dex } => {
                    anyhow!("missing Hyperliquid asset `{symbol}` in perp dex `{dex}`")
                }
            })?;
        let leverage_is_cross = leverage_is_cross(symbol, dex_ref, asset)?;
        let id = match dex_ref {
            PerpDexRef::Default => index as u32,
            PerpDexRef::Hip3 { dex } => {
                hip3_asset_id(self.perp_dex_index(dex).await?, index as u32)
            }
        };
        Ok(AssetDescriptor {
            id,
            sz_decimals: asset.sz_decimals,
            max_leverage: asset.max_leverage,
            leverage_is_cross,
        })
    }

    async fn perp_dex_index(&self, dex: &str) -> Result<u32> {
        if let Some(index) = self.perp_dex_index_cache.lock().await.get(dex).copied() {
            return Ok(index);
        }

        let dexes: Vec<Option<PerpDexMeta>> =
            self.post_info(&json!({ "type": "perpDexs" })).await?;
        let index = dexes
            .iter()
            .enumerate()
            .find_map(|(index, entry)| {
                entry
                    .as_ref()
                    .filter(|entry| entry.name == dex)
                    .map(|_| index as u32)
            })
            .ok_or_else(|| anyhow!("missing Hyperliquid perp dex `{dex}`"))?;
        self.perp_dex_index_cache
            .lock()
            .await
            .insert(dex.to_string(), index);
        Ok(index)
    }

    async fn post_info<T: DeserializeOwned>(&self, body: &serde_json::Value) -> Result<T> {
        self.post_json("/info", body).await
    }

    async fn post_exchange(&self, action: &ExchangeAction) -> Result<serde_json::Value> {
        let body = self.exchange_request_body(action)?;
        self.post_json("/exchange", &body).await
    }

    fn exchange_request_body(&self, action: &ExchangeAction) -> Result<serde_json::Value> {
        let nonce = (self.timestamp_provider)();
        let connection_id = action_hash(action, nonce, self.credentials.vault_address())?;
        let signature = sign_l1_action(self.credentials.private_key(), self.chain, connection_id)?;
        let mut body = json!({
            "action": action,
            "nonce": nonce,
            "signature": signature,
        });
        if let Some(vault_address) = self.credentials.vault_address() {
            body["vaultAddress"] = json!(vault_address);
        }
        Ok(body)
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let response = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .with_context(|| format!("request POST {path} failed"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read Hyperliquid response body for {path}"))?;
        if status != StatusCode::OK {
            return Err(HyperliquidRestError::http_status(path, status, body).into());
        }
        serde_json::from_str(&body).map_err(|error| {
            anyhow!("failed to deserialize Hyperliquid response for {path}: {error}; body: {body}")
        })
    }
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .context("failed to build Hyperliquid HTTP client")
}

fn validate_recent_history_times(
    endpoint: &str,
    start_time: i64,
    end_time: i64,
    times: impl IntoIterator<Item = i64>,
) -> Result<()> {
    let mut previous_time = None;
    for time in times {
        ensure!(
            (start_time..=end_time).contains(&time),
            "Hyperliquid {endpoint} returned timestamp {time} outside requested range {start_time}..={end_time}"
        );
        if let Some(previous_time) = previous_time {
            ensure!(
                time >= previous_time,
                "Hyperliquid {endpoint} returned timestamps out of order: {time} after {previous_time}"
            );
        }
        previous_time = Some(time);
    }
    Ok(())
}

fn next_recent_history_start(
    current_start: i64,
    item_count: usize,
    latest_time: Option<i64>,
    page_size: usize,
) -> Result<Option<i64>> {
    ensure!(
        page_size > 0,
        "Hyperliquid history page size must be positive"
    );
    ensure!(
        item_count <= page_size,
        "Hyperliquid history response exceeded expected page size {page_size}: received {item_count} items"
    );
    if item_count < page_size {
        return Ok(None);
    }
    let latest_time = latest_time.context("Hyperliquid full history page missing timestamp")?;
    // startTime is inclusive. Reusing the last timestamp preserves records that share the page
    // boundary millisecond; endpoint-specific identity maps absorb the repeated boundary items.
    ensure!(
        latest_time >= current_start,
        "Hyperliquid history pagination did not advance safely from {current_start}"
    );
    Ok(Some(latest_time))
}

fn ensure_recent_fill_history_complete(unique_fill_count: usize) -> Result<()> {
    ensure!(
        unique_fill_count < USER_FILLS_RETENTION_LIMIT,
        "Hyperliquid userFillsByTime reached the 10,000-fill retention limit; cannot prove the requested PNL window is complete"
    );
    Ok(())
}

fn usdc_total_balance(state: &SpotClearinghouseStateResponse) -> Result<f64> {
    parse_decimal("USDC.total", &usdc_balance(state)?.total)
}

fn parse_perp_dex(symbol: &str) -> Result<PerpDexRef<'_>> {
    match symbol.split_once(':') {
        None => Ok(PerpDexRef::Default),
        Some((dex, coin)) => {
            if coin.contains(':') || dex.is_empty() || coin.is_empty() {
                return Err(anyhow!(
                    "invalid Hyperliquid HIP-3 symbol `{symbol}`: expected `{{dex}}:{{coin}}`"
                ));
            }
            if !dex.chars().all(|value| value.is_ascii_graphic()) {
                return Err(anyhow!(
                    "invalid Hyperliquid HIP-3 symbol `{symbol}`: dex must contain only visible ASCII characters"
                ));
            }
            Ok(PerpDexRef::Hip3 { dex })
        }
    }
}

fn with_perp_dex(mut body: serde_json::Value, dex_ref: PerpDexRef<'_>) -> serde_json::Value {
    if let PerpDexRef::Hip3 { dex } = dex_ref {
        body["dex"] = json!(dex);
    }
    body
}

fn hip3_asset_id(perp_dex_index: u32, index_in_meta: u32) -> u32 {
    100_000 + perp_dex_index * 10_000 + index_in_meta
}

fn leverage_is_cross(symbol: &str, dex_ref: PerpDexRef<'_>, asset: &PerpAssetMeta) -> Result<bool> {
    if matches!(dex_ref, PerpDexRef::Default) {
        return Ok(true);
    }

    if asset.only_isolated.unwrap_or(false) {
        return Ok(false);
    }

    match asset.margin_mode.as_deref() {
        None => Ok(true),
        Some("noCross" | "strictIsolated") => Ok(false),
        Some(other) => Err(anyhow!(
            "unsupported Hyperliquid marginMode `{other}` for asset `{symbol}`"
        )),
    }
}

fn usdc_available_after_maintenance(state: &SpotClearinghouseStateResponse) -> Result<f64> {
    if let Some((_, value)) = state
        .token_to_available_after_maintenance
        .iter()
        .find(|(token, _)| *token == 0)
    {
        return parse_decimal("tokenToAvailableAfterMaintenance[USDC]", value);
    }

    let balance = usdc_balance(state)?;
    Ok(parse_decimal("USDC.total", &balance.total)? - parse_decimal("USDC.hold", &balance.hold)?)
}

fn usdc_balance(
    state: &SpotClearinghouseStateResponse,
) -> Result<&crate::rest::models::SpotBalance> {
    state
        .balances
        .iter()
        .find(|balance| balance.coin == "USDC" && balance.token == 0)
        .context("missing Hyperliquid unified USDC balance")
}

fn order_receipt_from_response(
    response: serde_json::Value,
    client_order_id: &str,
) -> Result<OrderReceipt> {
    if response["status"] != "ok" {
        return Err(HyperliquidRestError::exchange_response(response).into());
    }
    let status = response
        .pointer("/response/data/statuses/0")
        .context("missing Hyperliquid order status")?;
    if let Some(error) = status.get("error").and_then(serde_json::Value::as_str) {
        return Err(HyperliquidRestError::order_rejected(error).into());
    }
    if let Some(resting) = status.get("resting") {
        return Ok(OrderReceipt {
            order_id: required_u64(resting, "oid")?.to_string(),
            client_order_id: client_order_id.to_string(),
            filled_qty: 0.0,
            status: OrderStatus::New,
        });
    }
    if let Some(filled) = status.get("filled") {
        return Ok(OrderReceipt {
            order_id: required_u64(filled, "oid")?.to_string(),
            client_order_id: client_order_id.to_string(),
            filled_qty: filled
                .get("totalSz")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("0")
                .parse()
                .context("invalid Hyperliquid filled totalSz")?,
            status: OrderStatus::Filled,
        });
    }
    Err(anyhow!("unsupported Hyperliquid order status: {status}"))
}

fn ensure_exchange_ok(response: serde_json::Value) -> Result<()> {
    if response["status"] == "ok" {
        Ok(())
    } else {
        Err(HyperliquidRestError::exchange_response(response).into())
    }
}

fn required_u64(value: &serde_json::Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow!("missing Hyperliquid `{field}`"))
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid Hyperliquid decimal `{field}`: {value}"))
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

fn format_price_decimal(value: f64, sz_decimals: u32) -> String {
    format_decimal(normalize_perp_price(
        value,
        sz_decimals,
        PriceRounding::Nearest,
    ))
}

fn format_size_decimal(value: f64, sz_decimals: u32) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    format_decimal_at_scale(value, sz_decimals as usize)
}

fn format_decimal_at_scale(value: f64, decimals: usize) -> String {
    let factor = 10_f64.powi(decimals as i32);
    let rounded = (value * factor).round() / factor;
    trim_decimal_string(format!("{rounded:.decimals$}"))
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
    use std::sync::Arc;

    use poise_core::track::{Instrument, Venue};
    use poise_core::types::Side;
    use poise_engine::ledger::TrackPnlRecordKind;
    use poise_engine::ports::{OrderRequest as PortOrderRequest, OrderStatus};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    use super::{
        HyperliquidRestClient, PerpDexRef, build_http_client, ensure_recent_fill_history_complete,
        next_recent_history_start, parse_perp_dex,
    };
    use crate::config::Credentials;

    fn credentials() -> Credentials {
        crate::Config {
            deployment: crate::Deployment::Testnet,
            private_key: Some(
                "0xe908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e".to_string(),
            ),
            wallet_address: Some("0x2222222222222222222222222222222222222222".to_string()),
            vault_address: None,
        }
        .credentials()
        .unwrap()
    }

    fn vault_credentials() -> Credentials {
        crate::Config {
            deployment: crate::Deployment::Testnet,
            private_key: Some(
                "0xe908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e".to_string(),
            ),
            wallet_address: Some("0x2222222222222222222222222222222222222222".to_string()),
            vault_address: Some("0x3333333333333333333333333333333333333333".to_string()),
        }
        .credentials()
        .unwrap()
    }

    fn fill_value(
        coin: &str,
        time: i64,
        tid: u64,
        fee: &str,
        fee_token: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "coin": coin,
            "px": "65000.5",
            "sz": "0.02",
            "side": "B",
            "time": time,
            "closedPnl": "3.25",
            "hash": format!("0xfill{tid}"),
            "oid": 10_000_u64 + tid,
            "tid": tid,
            "fee": fee,
            "feeToken": fee_token,
        })
    }

    fn funding_value(coin: &str, time: i64, hash: &str, usdc: &str) -> serde_json::Value {
        serde_json::json!({
            "delta": {"type": "funding", "coin": coin, "usdc": usdc},
            "time": time,
            "hash": hash,
        })
    }

    fn full_fill_page(last: serde_json::Value) -> String {
        let mut values = (0_u64..1_999)
            .map(|index| fill_value("ETH", 1_700_000_000_000, 10_000 + index, "0.002", "HYPE"))
            .collect::<Vec<_>>();
        values.push(last);
        serde_json::to_string(&values).unwrap()
    }

    fn full_funding_page(last: serde_json::Value) -> String {
        let mut values = (0_u64..499)
            .map(|index| {
                funding_value(
                    &format!("ALT{index}"),
                    1_700_000_000_000 + index as i64,
                    &format!("0xfunding{index}"),
                    "-0.01",
                )
            })
            .collect::<Vec<_>>();
        values.push(last);
        serde_json::to_string(&values).unwrap()
    }

    #[test]
    fn parses_hyperliquid_default_and_hip3_symbols() {
        assert_eq!(parse_perp_dex("BTC").unwrap(), PerpDexRef::Default);
        assert_eq!(
            parse_perp_dex("xyz:CBRS").unwrap(),
            PerpDexRef::Hip3 { dex: "xyz" }
        );

        for symbol in ["xyz:", ":CBRS", "a:b:c"] {
            let error = parse_perp_dex(symbol).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("invalid Hyperliquid HIP-3 symbol")
            );
        }
    }

    #[tokio::test]
    async fn recent_track_pnl_records_map_and_filter_exact_symbol() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"[
                    {"coin":"ETH","px":"2376.0","sz":"0.0064","side":"B","time":1700080000000,"closedPnl":"-0.1","hash":"0xeth","oid":41,"tid":32,"fee":"0.002","feeToken":"HYPE"},
                    {"coin":"BTC","px":"65000.5","sz":"0.02","side":"A","time":1700080001000,"closedPnl":"3.25","hash":"0xbtc","oid":12345,"tid":999,"fee":"0.12","feeToken":"USDC"}
                ]"#,
            ),
            MockResponse::json(
                200,
                r#"[
                    {"delta":{"type":"funding","coin":"ETH","usdc":"-0.05"},"time":1700070000000,"hash":"0xeth-funding"},
                    {"delta":{"type":"funding","coin":"BTC","usdc":" -0.15"},"time":1700070001000,"hash":"0xbtc-funding"}
                ]"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let records = client.get_recent_track_pnl_records("BTC").await.unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind(), TrackPnlRecordKind::Funding);
        assert_eq!(
            records[0].instrument,
            Instrument::new(Venue::Hyperliquid, "BTC")
        );
        assert_eq!(
            records[0].source_key.as_deref(),
            Some("hyperliquid:funding:BTC:1700070001000")
        );
        assert_eq!(records[0].funding_amount().unwrap().value(), -0.15);
        assert_eq!(records[1].kind(), TrackPnlRecordKind::Trade);
        assert_eq!(
            records[1].source_key.as_deref(),
            Some("hyperliquid:fill:BTC:999")
        );
        assert_eq!(records[1].realized_pnl().unwrap().value(), 3.25);
        assert_eq!(records[1].trading_fee().unwrap().value(), 0.12);

        let requests = server.requests().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].json_body()["type"], "userFillsByTime");
        assert_eq!(requests[0].json_body()["startTime"], 1_700_000_000_000_i64);
        assert_eq!(requests[0].json_body()["endTime"], 1_700_086_400_000_i64);
        assert_eq!(requests[0].json_body()["aggregateByTime"], false);
        assert_eq!(requests[1].json_body()["type"], "userFunding");
        assert_eq!(requests[1].json_body()["startTime"], 1_700_000_000_000_i64);
        assert_eq!(requests[1].json_body()["endTime"], 1_700_086_400_000_i64);
    }

    #[tokio::test]
    async fn account_queries_target_vault_when_configured() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"125.5"},"withdrawable":"100.25","assetPositions":[]}"#,
            ),
            MockResponse::json(200, "[]"),
            MockResponse::json(200, "[]"),
            MockResponse::json(200, "[]"),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            vault_credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        client.get_position("BTC").await.unwrap();
        client.get_open_orders("BTC").await.unwrap();
        client.get_recent_track_pnl_records("BTC").await.unwrap();

        let requests = server.requests().await;
        assert_eq!(requests.len(), 4);
        assert!(requests.iter().all(|request| {
            request.json_body()["user"] == "0x3333333333333333333333333333333333333333"
        }));
    }

    #[tokio::test]
    async fn recent_track_pnl_records_preserve_non_usdc_fee_token() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"[{"coin":"BTC","px":"65000.5","sz":"0.02","side":"B","time":1700080001000,"closedPnl":"3.25","hash":"0xbtc","oid":12345,"tid":999,"fee":"0.12","feeToken":"HYPE"}]"#,
            ),
            MockResponse::json(200, "[]"),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let records = client.get_recent_track_pnl_records("BTC").await.unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].trading_fee().unwrap().value(), 0.12);
        assert_eq!(records[0].trading_fee().unwrap().asset().as_str(), "HYPE");
    }

    #[tokio::test]
    async fn recent_track_pnl_records_continue_from_last_full_page_timestamp() {
        let boundary_time = 1_700_000_000_500;
        let boundary_fill = fill_value("BTC", boundary_time, 999, "0.12", "USDC");
        let first_page = full_fill_page(boundary_fill.clone());
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, &first_page),
            MockResponse::json(
                200,
                &serde_json::json!([
                    boundary_fill,
                    fill_value("BTC", boundary_time, 1000, "0.13", "USDC"),
                ])
                .to_string(),
            ),
            MockResponse::json(200, "[]"),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let records = client.get_recent_track_pnl_records("BTC").await.unwrap();

        assert_eq!(
            records
                .iter()
                .map(|record| record.source_key.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["hyperliquid:fill:BTC:1000", "hyperliquid:fill:BTC:999"]
        );
        let requests = server.requests().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].json_body()["startTime"], boundary_time);
    }

    #[tokio::test]
    async fn recent_track_pnl_records_reject_full_page_without_new_fill_ids() {
        let boundary_time = 1_700_000_000_500;
        let page = serde_json::to_string(
            &(0_u64..2_000)
                .map(|tid| fill_value("BTC", boundary_time, tid, "0.12", "USDC"))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, &page),
            MockResponse::json(200, &page),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = client
            .get_recent_track_pnl_records("BTC")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("full userFillsByTime page"));
        assert!(error.to_string().contains("no new fill identity"));
    }

    #[tokio::test]
    async fn recent_track_pnl_records_reject_conflicting_fill_tid() {
        let boundary_time = 1_700_000_000_500;
        let boundary_fill = fill_value("BTC", boundary_time, 999, "0.12", "USDC");
        let first_page = full_fill_page(boundary_fill);
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, &first_page),
            MockResponse::json(
                200,
                &serde_json::json!([fill_value("BTC", boundary_time, 999, "0.99", "USDC")])
                    .to_string(),
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = client
            .get_recent_track_pnl_records("BTC")
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("conflicting Hyperliquid fill identity BTC:999")
        );
    }

    #[tokio::test]
    async fn recent_track_pnl_records_allow_more_than_four_funding_pages() {
        let mut responses = vec![MockResponse::json(200, "[]")];
        let first_time = 1_700_000_000_000_i64;
        for page_index in 0_i64..5 {
            let page_start = first_time + page_index * 499;
            let values = (0_i64..500)
                .map(|item_index| {
                    funding_value(
                        &format!("ALT{page_index}-{item_index}"),
                        page_start + item_index,
                        &format!("0xfunding-{page_index}-{item_index}"),
                        "-0.01",
                    )
                })
                .collect::<Vec<_>>();
            responses.push(MockResponse::json(
                200,
                &serde_json::to_string(&values).unwrap(),
            ));
        }
        responses.push(MockResponse::json(200, "[]"));
        let server = MockHttpServer::spawn(responses).await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let records = client.get_recent_track_pnl_records("BTC").await.unwrap();

        assert!(records.is_empty());
        assert_eq!(server.requests().await.len(), 7);
    }

    #[tokio::test]
    async fn recent_track_pnl_records_allow_same_tid_for_different_symbols() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                &serde_json::json!([
                    fill_value("ETH", 1_700_000_000_500, 999, "0.01", "USDC"),
                    fill_value("BTC", 1_700_000_000_501, 999, "0.12", "USDC")
                ])
                .to_string(),
            ),
            MockResponse::json(200, "[]"),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let records = client.get_recent_track_pnl_records("BTC").await.unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].source_key.as_deref(),
            Some("hyperliquid:fill:BTC:999")
        );
    }

    #[tokio::test]
    async fn recent_track_pnl_records_reject_conflicting_funding_canonical_key() {
        let boundary_time = 1_700_000_000_500;
        let first_page =
            full_funding_page(funding_value("BTC", boundary_time, "0xfunding-a", "-0.15"));
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, "[]"),
            MockResponse::json(200, &first_page),
            MockResponse::json(
                200,
                &serde_json::json!([funding_value("BTC", boundary_time, "0xfunding-b", "-0.16")])
                    .to_string(),
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = client
            .get_recent_track_pnl_records("BTC")
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("conflicting Hyperliquid funding canonical key BTC:1700000000500")
        );
    }

    #[tokio::test]
    async fn recent_track_pnl_records_deduplicate_funding_at_inclusive_boundary() {
        let boundary_time = 1_700_000_000_500;
        let boundary_funding = funding_value("BTC", boundary_time, "0xbtc-boundary", "-0.15");
        let first_page = full_funding_page(boundary_funding.clone());
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, "[]"),
            MockResponse::json(200, &first_page),
            MockResponse::json(
                200,
                &serde_json::json!([
                    boundary_funding,
                    funding_value("BTC", boundary_time + 1, "0xbtc-next", "0.05")
                ])
                .to_string(),
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let records = client.get_recent_track_pnl_records("BTC").await.unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].funding_amount().unwrap().value(), -0.15);
        assert_eq!(records[1].funding_amount().unwrap().value(), 0.05);
        let requests = server.requests().await;
        assert_eq!(requests[2].json_body()["startTime"], boundary_time);
    }

    #[tokio::test]
    async fn recent_track_pnl_records_allow_reused_funding_hash_across_canonical_keys() {
        let boundary_time = 1_700_000_000_500;
        let first_page = full_funding_page(funding_value(
            "BTC",
            boundary_time,
            "0xshared-funding",
            "-0.15",
        ));
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, "[]"),
            MockResponse::json(200, &first_page),
            MockResponse::json(
                200,
                &serde_json::json!([funding_value(
                    "BTC",
                    boundary_time + 1,
                    "0xshared-funding",
                    "-0.16"
                )])
                .to_string(),
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let records = client.get_recent_track_pnl_records("BTC").await.unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].funding_amount().unwrap().value(), -0.15);
        assert_eq!(records[1].funding_amount().unwrap().value(), -0.16);
    }

    #[tokio::test]
    async fn recent_track_pnl_records_accept_whitespace_around_usdc_fee_token() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                &serde_json::json!([fill_value("BTC", 1_700_000_000_500, 999, "0.12", " USDC ")])
                    .to_string(),
            ),
            MockResponse::json(200, "[]"),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let records = client.get_recent_track_pnl_records("BTC").await.unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].trading_fee().unwrap().value(), 0.12);
        assert_eq!(records[0].trading_fee().unwrap().asset().as_str(), "USDC");
    }

    #[tokio::test]
    async fn recent_track_pnl_records_reject_missing_funding_hash() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, "[]"),
            MockResponse::json(
                200,
                r#"[{"delta":{"type":"funding","coin":"BTC","usdc":"-0.15"},"time":1700081001000}]"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = client
            .get_recent_track_pnl_records("BTC")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("failed to deserialize"));
        assert!(error.to_string().contains("hash"));
    }

    #[tokio::test]
    async fn recent_track_pnl_records_keep_transient_http_error_distinct() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            503,
            r#"{"error":"temporarily unavailable"}"#,
        )])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_086_400_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = client
            .get_recent_track_pnl_records("BTC")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("status 503"));
    }

    #[test]
    fn recent_history_pagination_uses_inclusive_cursor_for_full_pages() {
        assert_eq!(
            next_recent_history_start(1_000, 1_999, Some(1_500), 2_000).unwrap(),
            None
        );
        assert_eq!(
            next_recent_history_start(1_000, 2_000, Some(1_500), 2_000).unwrap(),
            Some(1_500)
        );
        assert_eq!(
            next_recent_history_start(1_000, 500, Some(2_000), 500).unwrap(),
            Some(2_000)
        );
        assert!(
            next_recent_history_start(1_000, 500, Some(999), 500)
                .unwrap_err()
                .to_string()
                .contains("did not advance")
        );
    }

    #[test]
    fn recent_fill_history_rejects_the_exchange_retention_boundary() {
        ensure_recent_fill_history_complete(9_999).unwrap();
        let error = ensure_recent_fill_history_complete(10_000).unwrap_err();
        assert!(error.to_string().contains("10,000-fill retention limit"));
    }

    #[tokio::test]
    async fn info_queries_post_expected_info_requests_and_map_responses() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, r#"{"universe":[{"name":"BTC","szDecimals":5}]}"#),
            MockResponse::json(200, r#""disabled""#),
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"125.5"},"withdrawable":"100.25","assetPositions":[{"position":{"coin":"BTC","szi":"0.02","entryPx":"65000.5","unrealizedPnl":"3.25"}}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"125.5"},"withdrawable":"100.25","assetPositions":[{"position":{"coin":"BTC","szi":"0.02","entryPx":"65000.5","unrealizedPnl":"3.25"}}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"125.5"},"withdrawable":"100.25","assetPositions":[{"position":{"coin":"BTC","szi":"0.02","entryPx":"65000.5","unrealizedPnl":"3.25"}}]}"#,
            ),
            MockResponse::json(
                200,
                r#"[{"coin":"BTC","oid":12345,"cloid":"0x11111111111111111111111111111111","side":"B","limitPx":"65000.5","sz":"0.02"}]"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_000_000_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let info = client.get_exchange_info("BTC").await.unwrap();
        let summary = client.get_account_summary().await.unwrap();
        let position = client.get_position("BTC").await.unwrap();
        let capacity = client.get_account_capacity_snapshot(10).await.unwrap();
        let open_orders = client.get_open_orders("BTC").await.unwrap();

        assert_eq!(
            info.instrument_spec.instrument(),
            &Instrument::new(Venue::Hyperliquid, "BTC")
        );
        assert_eq!(summary.equity().value(), 125.5);
        assert_eq!(position.qty, 0.02);
        assert_eq!(capacity.max_increase_notional, 1002.5);
        assert_eq!(open_orders.len(), 1);
        let requests = server.requests().await;
        assert_eq!(requests[0].path, "/info");
        assert_eq!(requests[0].json_body()["type"], "meta");
        assert_eq!(requests[1].json_body()["type"], "userAbstraction");
        assert_eq!(requests[2].json_body()["type"], "clearinghouseState");
        assert_eq!(
            requests[2].json_body()["user"],
            "0x2222222222222222222222222222222222222222"
        );
        assert_eq!(requests[3].json_body()["type"], "clearinghouseState");
        assert_eq!(requests[4].json_body()["type"], "clearinghouseState");
        assert_eq!(requests[5].json_body()["type"], "openOrders");
    }

    #[tokio::test]
    async fn unified_account_summary_uses_spot_usdc_as_balance_source_of_truth() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, r#""unifiedAccount""#),
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"300.509522"},"withdrawable":"0.0","assetPositions":[{"position":{"coin":"BTC","szi":"-0.02034","entryPx":"78805.1","unrealizedPnl":"-30.85317"}}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"balances":[{"coin":"USDC","token":0,"total":"891.55684101","hold":"314.896004","entryNtl":"0.0"}],"tokenToAvailableAfterMaintenance":[[0,"871.15175301"]]}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_000_000_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let summary = client.get_account_summary().await.unwrap();

        assert_eq!(summary.valuation_asset().as_str(), "USDC");
        assert_eq!(summary.equity().value(), 891.55684101);
        assert_eq!(summary.available().unwrap().value(), 871.15175301);
        assert_eq!(summary.unrealized_pnl().unwrap().value(), -30.85317);
        let requests = server.requests().await;
        assert_eq!(requests[0].json_body()["type"], "userAbstraction");
        assert_eq!(requests[1].json_body()["type"], "clearinghouseState");
        assert_eq!(requests[2].json_body()["type"], "spotClearinghouseState");
    }

    #[tokio::test]
    async fn unified_account_capacity_uses_spot_usdc_available_after_maintenance() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, r#""unifiedAccount""#),
            MockResponse::json(
                200,
                r#"{"balances":[{"coin":"USDC","token":0,"total":"891.55684101","hold":"314.896004","entryNtl":"0.0"}],"tokenToAvailableAfterMaintenance":[[0,"871.15175301"]]}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_000_000_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let capacity = client.get_account_capacity_snapshot(20).await.unwrap();

        assert_eq!(capacity.max_increase_notional, 17_423.0350602);
        let requests = server.requests().await;
        assert_eq!(requests[0].json_body()["type"], "userAbstraction");
        assert_eq!(requests[1].json_body()["type"], "spotClearinghouseState");
    }

    #[tokio::test]
    async fn cached_info_queries_avoid_repeated_high_weight_requests() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(200, r#""unifiedAccount""#),
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"300.509522"},"withdrawable":"0.0","assetPositions":[]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"balances":[{"coin":"USDC","token":0,"total":"891.55684101","hold":"314.896004","entryNtl":"0.0"}],"tokenToAvailableAfterMaintenance":[[0,"871.15175301"]]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"301.0"},"withdrawable":"0.0","assetPositions":[]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"balances":[{"coin":"USDC","token":0,"total":"892.0","hold":"314.0","entryNtl":"0.0"}],"tokenToAvailableAfterMaintenance":[[0,"872.0"]]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67890}}]}}}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67891}}]}}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        client.get_account_summary().await.unwrap();
        client.get_account_summary().await.unwrap();
        for client_order_id in ["bc-first", "bc-second"] {
            client
                .submit_order(PortOrderRequest {
                    instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                    side: Side::Buy,
                    price: 2000.0,
                    quantity: 3.5,
                    client_order_id: client_order_id.to_string(),
                    reduce_only: false,
                })
                .await
                .unwrap();
        }

        let requests = server.requests().await;
        let info_types = requests
            .iter()
            .filter(|request| request.path == "/info")
            .map(|request| request.json_body()["type"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            info_types,
            vec![
                "userAbstraction",
                "clearinghouseState",
                "spotClearinghouseState",
                "clearinghouseState",
                "spotClearinghouseState",
                "meta",
            ]
        );
    }

    #[tokio::test]
    async fn meta_cache_is_scoped_by_perp_dex() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"BTC","szDecimals":5,"maxLeverage":40}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"xyz:CBRS","szDecimals":2,"maxLeverage":10,"onlyIsolated":true,"marginMode":"noCross"}]}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_000_000_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let default_info = client.get_exchange_info("BTC").await.unwrap();
        let hip3_info = client.get_exchange_info("xyz:CBRS").await.unwrap();
        client.get_exchange_info("BTC").await.unwrap();
        client.get_exchange_info("xyz:CBRS").await.unwrap();

        assert_eq!(
            default_info.instrument_spec.instrument(),
            &Instrument::new(Venue::Hyperliquid, "BTC")
        );
        assert_eq!(
            hip3_info.instrument_spec.instrument(),
            &Instrument::new(Venue::Hyperliquid, "xyz:CBRS")
        );
        let requests = server.requests().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].json_body()["type"], "meta");
        assert!(requests[0].json_body().get("dex").is_none());
        assert_eq!(requests[1].json_body()["type"], "meta");
        assert_eq!(requests[1].json_body()["dex"], "xyz");
    }

    #[tokio::test]
    async fn hip3_leverage_uses_builder_asset_id_margin_mode_and_max_leverage() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"xyz:XYZ100","szDecimals":2,"maxLeverage":10},{"name":"xyz:CBRS","szDecimals":2,"maxLeverage":10,"onlyIsolated":true,"marginMode":"noCross"}]}"#,
            ),
            MockResponse::json(200, r#"[null,{"name":"xyz","fullName":"XYZ"}]"#),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"updateLeverage"}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        client.set_leverage("xyz:CBRS", 3).await.unwrap();
        let error = client.set_leverage("xyz:CBRS", 11).await.unwrap_err();

        assert!(error.to_string().contains("exceeds maxLeverage 10x"));
        let requests = server.requests().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].json_body()["type"], "meta");
        assert_eq!(requests[0].json_body()["dex"], "xyz");
        assert_eq!(requests[1].json_body()["type"], "perpDexs");
        assert_eq!(requests[2].json_body()["action"]["type"], "updateLeverage");
        assert_eq!(requests[2].json_body()["action"]["asset"], 110001);
        assert_eq!(requests[2].json_body()["action"]["isCross"], false);
        assert_eq!(requests[2].json_body()["action"]["leverage"], 3);
    }

    #[tokio::test]
    async fn hip3_leverage_allows_cross_when_margin_mode_is_unspecified() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"xyz:TSLA","szDecimals":2,"maxLeverage":10}]}"#,
            ),
            MockResponse::json(200, r#"[null,{"name":"xyz","fullName":"XYZ"}]"#),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"updateLeverage"}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        client.set_leverage("xyz:TSLA", 3).await.unwrap();

        let requests = server.requests().await;
        assert_eq!(requests[2].json_body()["action"]["asset"], 110000);
        assert_eq!(requests[2].json_body()["action"]["isCross"], true);
    }

    #[tokio::test]
    async fn hip3_leverage_fails_closed_for_unknown_margin_mode() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"universe":[{"name":"xyz:ODD","szDecimals":2,"maxLeverage":10,"marginMode":"futureMode"}]}"#,
        )])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = client.set_leverage("xyz:ODD", 3).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported Hyperliquid marginMode")
        );
        let requests = server.requests().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].json_body()["type"], "meta");
    }

    #[tokio::test]
    async fn hip3_submit_cancel_and_cancel_all_use_builder_asset_id() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"xyz:XYZ100","szDecimals":2,"maxLeverage":10},{"name":"xyz:CBRS","szDecimals":2,"maxLeverage":10,"onlyIsolated":true,"marginMode":"noCross"}]}"#,
            ),
            MockResponse::json(200, r#"[null,{"name":"xyz","fullName":"XYZ"}]"#),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67890}}]}}}"#,
            ),
            MockResponse::json(200, r#"{"status":"ok","response":{"type":"cancel"}}"#),
            MockResponse::json(
                200,
                r#"[{"coin":"xyz:CBRS","oid":12345,"side":"B","limitPx":"100.5","sz":"2"}]"#,
            ),
            MockResponse::json(200, r#"{"status":"ok","response":{"type":"cancel"}}"#),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        client
            .submit_order(PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "xyz:CBRS"),
                side: Side::Buy,
                price: 100.5,
                quantity: 2.0,
                client_order_id: "bc-hip3-submit".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();
        client.cancel_order("xyz:CBRS", "67890").await.unwrap();
        client.cancel_all("xyz:CBRS").await.unwrap();

        let requests = server.requests().await;
        assert_eq!(requests[0].json_body()["type"], "meta");
        assert_eq!(requests[0].json_body()["dex"], "xyz");
        assert_eq!(requests[1].json_body()["type"], "perpDexs");
        assert_eq!(requests[2].json_body()["action"]["orders"][0]["a"], 110001);
        assert_eq!(requests[3].json_body()["action"]["cancels"][0]["a"], 110001);
        assert_eq!(requests[4].json_body()["type"], "openOrders");
        assert_eq!(requests[4].json_body()["dex"], "xyz");
        assert_eq!(requests[5].json_body()["action"]["cancels"][0]["a"], 110001);
        assert_eq!(requests[5].json_body()["action"]["cancels"][0]["o"], 12345);
    }

    #[tokio::test]
    async fn hip3_state_queries_use_dex_but_account_capacity_uses_account_scope() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"125.5"},"withdrawable":"100.25","assetPositions":[{"position":{"coin":"xyz:CBRS","szi":"2.0","entryPx":"100.5","unrealizedPnl":"3.25"}}]}"#,
            ),
            MockResponse::json(
                200,
                r#"[{"coin":"xyz:CBRS","oid":12345,"side":"B","limitPx":"100.5","sz":"2"}]"#,
            ),
            MockResponse::json(200, r#""disabled""#),
            MockResponse::json(
                200,
                r#"{"marginSummary":{"accountValue":"125.5"},"withdrawable":"100.25","assetPositions":[]}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_700_000_000_000),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let position = client.get_position("xyz:CBRS").await.unwrap();
        let open_orders = client.get_open_orders("xyz:CBRS").await.unwrap();
        let capacity = client.get_account_capacity_snapshot(3).await.unwrap();

        assert_eq!(
            position.instrument,
            Instrument::new(Venue::Hyperliquid, "xyz:CBRS")
        );
        assert_eq!(position.qty, 2.0);
        assert_eq!(open_orders.len(), 1);
        assert_eq!(capacity.max_increase_notional, 300.75);
        let requests = server.requests().await;
        assert_eq!(requests[0].json_body()["type"], "clearinghouseState");
        assert_eq!(requests[0].json_body()["dex"], "xyz");
        assert_eq!(requests[1].json_body()["type"], "openOrders");
        assert_eq!(requests[1].json_body()["dex"], "xyz");
        assert_eq!(requests[2].json_body()["type"], "userAbstraction");
        assert!(requests[2].json_body().get("dex").is_none());
        assert_eq!(requests[3].json_body()["type"], "clearinghouseState");
        assert!(requests[3].json_body().get("dex").is_none());
    }

    #[tokio::test]
    async fn submit_order_posts_signed_order_action_and_maps_resting_status() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67890}}]}}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let receipt = client
            .submit_order(PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: "0x1e60610f0b3d420597c88c1fed2ad5ee".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();

        assert_eq!(receipt.order_id, "67890");
        assert_eq!(
            receipt.client_order_id,
            "0x1e60610f0b3d420597c88c1fed2ad5ee"
        );
        assert_eq!(receipt.status, OrderStatus::New);
        let requests = server.requests().await;
        assert_eq!(requests[1].path, "/exchange");
        let body = requests[1].json_body();
        assert_eq!(body["nonce"], 1_583_838);
        assert_eq!(body["action"]["type"], "order");
        assert_eq!(body["action"]["orders"][0]["a"], 1);
        assert_eq!(body["action"]["orders"][0]["b"], true);
        assert_eq!(body["action"]["orders"][0]["p"], "2000");
        assert_eq!(body["signature"]["v"], 27);
        assert!(body.get("vaultAddress").is_none());
    }

    #[tokio::test]
    async fn submit_order_does_not_follow_permanent_redirect() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::redirect(308, "/redirected-submit"),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67890}}]}}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            build_http_client().unwrap(),
        );

        let result = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: false,
            },
        )
        .await;

        let requests = server.requests().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].path, "/exchange");
        assert_eq!(
            result.unwrap_err().kind(),
            poise_engine::ports::ExecutionPortErrorKind::Failed
        );
    }

    #[tokio::test]
    async fn submit_order_trims_binary_float_noise_from_wire_decimals() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67890}}]}}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        client
            .submit_order(PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "ETH"),
                side: Side::Sell,
                price: 2356.4,
                quantity: 0.13140000000000002,
                client_order_id: "bc-67ceddd7d1a94ebb8bbe0ffb8e1f5f0f".to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();

        let requests = server.requests().await;
        let order = &requests[1].json_body()["action"]["orders"][0];
        assert_eq!(order["p"], "2356.4");
        assert_eq!(order["s"], "0.1314");
    }

    #[tokio::test]
    async fn submit_order_formats_price_to_hyperliquid_significant_figure_rule() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"BTC","szDecimals":5},{"name":"ETH","szDecimals":4}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67890}}]}}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        client
            .submit_order(PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "ETH"),
                side: Side::Buy,
                price: 2359.19,
                quantity: 0.0063,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: true,
            })
            .await
            .unwrap();

        let requests = server.requests().await;
        let order = &requests[1].json_body()["action"]["orders"][0];
        assert_eq!(order["a"], 1);
        assert_eq!(order["p"], "2359.2");
        assert_eq!(order["s"], "0.0063");
    }

    #[tokio::test]
    async fn submit_order_maps_internal_client_order_id_to_hyperliquid_cloid() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67890}}]}}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let internal_client_order_id = "bc-56961625d79c44978c760c53fda4eefc";
        let receipt = client
            .submit_order(PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: internal_client_order_id.to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();

        assert_eq!(receipt.client_order_id, internal_client_order_id);
        let requests = server.requests().await;
        let body = requests[1].json_body();
        let cloid = body["action"]["orders"][0]["c"]
            .as_str()
            .expect("Hyperliquid cloid must be serialized as a string");
        assert_ne!(cloid, internal_client_order_id);
        assert_eq!(cloid.len(), 34);
        assert!(cloid.starts_with("0x"));
        assert!(cloid[2..].chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn open_orders_map_registered_hyperliquid_cloid_back_to_internal_client_order_id() {
        let internal_client_order_id = "bk-56961625d79c44978c760c53fda4eefc";
        let exchange_cloid = crate::client_order_id::hyperliquid_cloid(internal_client_order_id);
        let open_orders = format!(
            r#"[{{"coin":"BTC","oid":67890,"cloid":"{exchange_cloid}","side":"B","limitPx":"2000","sz":"3.5"}}]"#
        );
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":67890}}]}}}"#,
            ),
            MockResponse::json(200, &open_orders),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        client
            .submit_order(PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: internal_client_order_id.to_string(),
                reduce_only: false,
            })
            .await
            .unwrap();

        let open_orders = client.get_open_orders("BTC").await.unwrap();

        assert_eq!(open_orders.len(), 1);
        assert_eq!(open_orders[0].client_order_id, internal_client_order_id);
    }

    #[tokio::test]
    async fn cancel_order_cancel_all_and_set_leverage_post_exchange_actions() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(200, r#"{"status":"ok","response":{"type":"cancel"}}"#),
            MockResponse::json(
                200,
                r#"[{"coin":"BTC","oid":12345,"side":"B","limitPx":"65000.5","sz":"0.02"}]"#,
            ),
            MockResponse::json(200, r#"{"status":"ok","response":{"type":"cancel"}}"#),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"updateLeverage"}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let cancel_receipt = client.cancel_order("BTC", "67890").await.unwrap();
        client.cancel_all("BTC").await.unwrap();
        client.set_leverage("BTC", 10).await.unwrap();

        assert_eq!(cancel_receipt.order_id, "67890");
        assert_eq!(cancel_receipt.status, OrderStatus::Canceled);
        let requests = server.requests().await;
        assert_eq!(requests[1].json_body()["action"]["type"], "cancel");
        assert_eq!(requests[1].json_body()["action"]["cancels"][0]["a"], 1);
        assert_eq!(requests[1].json_body()["action"]["cancels"][0]["o"], 67890);
        assert_eq!(requests[3].json_body()["action"]["type"], "cancel");
        assert_eq!(requests[3].json_body()["action"]["cancels"][0]["o"], 12345);
        assert_eq!(requests[4].json_body()["action"]["type"], "updateLeverage");
        assert_eq!(requests[4].json_body()["action"]["asset"], 1);
        assert_eq!(requests[4].json_body()["action"]["isCross"], true);
        assert_eq!(requests[4].json_body()["action"]["leverage"], 10);
    }

    #[tokio::test]
    async fn execution_port_maps_tick_size_rejection_to_execution_kind() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[{"error":"Price must be divisible by tick size. asset=1"}]}}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.01,
                quantity: 3.5,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::InvalidPriceIncrement
        );
        assert!(error.to_string().contains("tick size"));
    }

    #[tokio::test]
    async fn execution_port_maps_submit_http_500_to_outcome_unknown() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(500, r#"{"error":"gateway failure"}"#),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::SubmitOutcomeUnknown
        );
    }

    #[tokio::test]
    async fn execution_port_maps_malformed_submit_success_to_outcome_unknown() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(200, "{"),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::SubmitOutcomeUnknown
        );
    }

    #[tokio::test]
    async fn execution_port_maps_uninterpretable_submit_receipt_to_outcome_unknown() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(
                200,
                r#"{"status":"ok","response":{"type":"order","data":{"statuses":[]}}}"#,
            ),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::SubmitOutcomeUnknown
        );
    }

    #[tokio::test]
    async fn execution_port_keeps_explicit_exchange_rejection_final() {
        let server = MockHttpServer::spawn(vec![
            MockResponse::json(
                200,
                r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
            ),
            MockResponse::json(200, r#"{"status":"err","response":"invalid nonce"}"#),
        ])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::Failed
        );
    }

    #[tokio::test]
    async fn execution_port_keeps_metadata_failure_definitive_before_submit_dispatch() {
        let server =
            MockHttpServer::spawn(vec![MockResponse::json(500, r#"{"error":"meta down"}"#)]).await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );

        let error = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::Failed
        );
    }

    #[tokio::test]
    async fn execution_port_keeps_submit_connect_failure_definitive() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"universe":[{"name":"ETH","szDecimals":4},{"name":"BTC","szDecimals":5}]}"#,
        )])
        .await;
        let client = HyperliquidRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            credentials(),
            Arc::new(|| 1_583_838),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );
        client.asset_descriptor("BTC").await.unwrap();
        tokio::task::yield_now().await;

        let error = poise_engine::ports::ExecutionPort::submit_order(
            &client,
            PortOrderRequest {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                side: Side::Buy,
                price: 2000.0,
                quantity: 3.5,
                client_order_id: "bk-f254f5816fca4a7faa0455d6f14c0872".to_string(),
                reduce_only: false,
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.kind(),
            poise_engine::ports::ExecutionPortErrorKind::Failed
        );
    }

    #[derive(Debug, Clone)]
    struct MockResponse {
        status: u16,
        body: String,
        location: Option<String>,
    }

    impl MockResponse {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
                location: None,
            }
        }

        fn redirect(status: u16, location: &str) -> Self {
            Self {
                status,
                body: String::new(),
                location: Some(location.to_string()),
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
                    let response = {
                        let mut queue = queued_responses.lock().await;
                        queue.pop_front()
                    };

                    let Some(response) = response else {
                        break;
                    };

                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 1024];

                    loop {
                        let read = socket.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if request_complete(&buffer) {
                            break;
                        }
                    }

                    let request = parse_request(&String::from_utf8_lossy(&buffer));
                    stored_requests.lock().await.push(request);
                    let location = response
                        .location
                        .as_deref()
                        .map(|location| format!("location: {location}\r\n"))
                        .unwrap_or_default();
                    let reply = format!(
                        "HTTP/1.1 {} OK\r\n{}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response.status,
                        location,
                        response.body.len(),
                        response.body
                    );
                    socket.write_all(reply.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        path: String,
        body: String,
        headers: HashMap<String, String>,
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
            .find_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
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
        let path = request_line.split_whitespace().nth(1).unwrap().to_string();
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
            path,
            body,
            headers,
        }
    }
}
