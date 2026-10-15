//! 交易所客户端：端点、nonce、动作签名、合约规则的缓存。
//!
//! 身份是「账户地址 + API 钱包私钥」：读接口用账户地址，写接口用 API 钱包签名。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use poise_runtime::{Address, ExchangeError, Network, SecretKey};
use poise_strategy::instrument::Instrument;
use poise_strategy::units::Price;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::actions::ExchangeAction;
use crate::meta::AssetMeta;
use crate::models::{AssetContext, MetaResponse};
use crate::signing::{action_hash, sign_l1_action};
use crate::transport::{EXCHANGE_PATH, HttpTransport, INFO_PATH, Transport, build_http_client};

/// 正式网端点。
const MAINNET_BASE_URL: &str = "https://api.hyperliquid.xyz";
/// 测试网端点。
const TESTNET_BASE_URL: &str = "https://api.hyperliquid-testnet.xyz";

/// 交易所时间，毫秒。nonce 用它。
pub(crate) type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub struct HyperliquidConfig {
    pub network: Network,
    /// 账户地址（主钱包）。读接口查的是它。
    pub account_address: Address,
    /// API 钱包私钥。写接口用它签名。
    pub api_wallet_private_key: SecretKey,
    /// 覆盖交易所地址，只给测试用。
    pub base_url: Option<String>,
}

pub struct Hyperliquid {
    transport: Arc<dyn Transport>,
    network: Network,
    account_address: Address,
    api_wallet_private_key: SecretKey,
    clock: Clock,
    /// 上一个用过的 nonce。同一毫秒内连下几单时必须仍然递增，否则会被判重复。
    nonce: Mutex<u64>,
    /// 合约规则。一个进程内不变，取一次就够。
    assets: Mutex<Option<HashMap<String, AssetMeta>>>,
}

impl Hyperliquid {
    pub fn new(config: HyperliquidConfig) -> Result<Self, ExchangeError> {
        let base_url = config
            .base_url
            .unwrap_or_else(|| base_url_of(config.network).to_string());
        Ok(Self {
            transport: Arc::new(HttpTransport::new(base_url, build_http_client()?)),
            network: config.network,
            account_address: config.account_address,
            api_wallet_private_key: config.api_wallet_private_key,
            clock: Arc::new(|| Utc::now().timestamp_millis().max(0) as u64),
            nonce: Mutex::new(0),
            assets: Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_transport(
        transport: Arc<dyn Transport>,
        network: Network,
        account_address: Address,
        api_wallet_private_key: SecretKey,
        clock: Clock,
    ) -> Self {
        Self {
            transport,
            network,
            account_address,
            api_wallet_private_key,
            clock,
            nonce: Mutex::new(0),
            assets: Mutex::new(None),
        }
    }

    /// 读接口里的 `user` 字段：账户地址。`Address` 本身不实现 `Serialize`，
    /// 这里显式转成小写十六进制。
    pub(crate) fn user(&self) -> String {
        self.account_address.to_string()
    }

    pub(crate) fn now_ms(&self) -> u64 {
        (self.clock)()
    }

    /// 一个单调递增的 nonce。
    fn next_nonce(&self) -> u64 {
        let now = self.now_ms();
        let mut last = self.nonce.lock().expect("nonce lock poisoned");
        let nonce = now.max(last.saturating_add(1));
        *last = nonce;
        nonce
    }

    /// 读接口。要求响应是 `shape` 描述的形状。
    ///
    /// 解析失败时报的是缺哪个字段、期望什么类型、在哪一行（serde 的说明里带着位置）。
    /// 说明里可能引到出错的那个值——那是这个账户自己的数据，不是凭据。
    pub(crate) async fn info<T: DeserializeOwned>(
        &self,
        body: &Value,
        shape: &str,
    ) -> Result<T, ExchangeError> {
        let response = self.transport.post(INFO_PATH, body).await?;
        serde_json::from_value(response).map_err(|error| ExchangeError::Unavailable {
            reason: format!(
                "cannot parse the `{}` response as {shape}: {}",
                request_type(body),
                error
            ),
        })
    }

    /// 写接口：签名后 POST，返回交易所的原始响应。
    ///
    /// `expires_after` 是交易所侧的有效期（毫秒时间戳）。它约束的是**动作的送达**，
    /// 不是挂单的寿命：实测（测试网）过期才送到的动作会被拒（`Action already expired`），
    /// 而按有效期挂上去的单到点并不会被交易所撤掉。所以它回答的是「这张单还来不来得及
    /// 生效」——运行时的「结果未知」处理正需要这一点（见 `Runtime::resume`）。
    ///
    /// 它同时进两处——动作哈希的尾部（见 [`action_hash`]）和请求体顶层的 `expiresAfter`，
    /// 两处必须一致，否则交易所算出的哈希与我们签的对不上，只会回一句
    /// `User or API Wallet 0x… does not exist`。
    pub(crate) async fn exchange(
        &self,
        action: &ExchangeAction,
        expires_after: Option<u64>,
    ) -> Result<Value, ExchangeError> {
        let nonce = self.next_nonce();
        let connection_id =
            action_hash(action, nonce, expires_after).map_err(|error| local_failure(&error))?;
        let signature = sign_l1_action(&self.api_wallet_private_key, self.network, connection_id)
            .map_err(|error| local_failure(&error))?;
        let body = match expires_after {
            Some(expires_after) => json!({
                "action": action,
                "nonce": nonce,
                "signature": signature,
                "expiresAfter": expires_after,
            }),
            None => json!({
                "action": action,
                "nonce": nonce,
                "signature": signature,
            }),
        };
        tracing::debug!(
            action = action.kind(),
            nonce,
            expires_after,
            "posting an exchange action"
        );
        self.transport.post(EXCHANGE_PATH, &body).await
    }

    pub(crate) async fn mark_price(&self, instrument: &Instrument) -> Result<Price, ExchangeError> {
        let (meta, contexts): (MetaResponse, Vec<AssetContext>) = self
            .info(&json!({ "type": "metaAndAssetCtxs" }), "metaAndAssetCtxs")
            .await?;
        let index = meta
            .universe
            .iter()
            .position(|asset| asset.name == instrument.symbol())
            .ok_or_else(|| unknown_instrument(instrument))?;
        let context = contexts
            .get(index)
            .ok_or_else(|| ExchangeError::Unavailable {
                reason: format!(
                    "the exchange did not return market data for `{}`",
                    instrument.symbol()
                ),
            })?;
        Price::parse(&context.mark_px).map_err(|_| ExchangeError::Unavailable {
            reason: format!(
                "the exchange returned an invalid mark price for `{}`",
                instrument.symbol()
            ),
        })
    }

    /// 一个标的的合约规则，按需取一次。
    pub(crate) async fn asset(&self, instrument: &Instrument) -> Result<AssetMeta, ExchangeError> {
        let cached = {
            let assets = self.assets.lock().expect("asset lock poisoned");
            assets
                .as_ref()
                .and_then(|assets| assets.get(instrument.symbol()).copied())
        };
        if let Some(asset) = cached {
            return Ok(asset);
        }

        let meta: MetaResponse = self.info(&json!({ "type": "meta" }), "meta").await?;
        let index: HashMap<String, AssetMeta> = meta
            .universe
            .iter()
            .enumerate()
            .map(|(index, asset)| {
                (
                    asset.name.clone(),
                    AssetMeta::from_response(index as u32, asset),
                )
            })
            .collect();
        let found = index.get(instrument.symbol()).copied();
        *self.assets.lock().expect("asset lock poisoned") = Some(index);
        found.ok_or_else(|| unknown_instrument(instrument))
    }
}

fn base_url_of(network: Network) -> &'static str {
    match network {
        Network::Mainnet => MAINNET_BASE_URL,
        Network::Testnet => TESTNET_BASE_URL,
    }
}

fn unknown_instrument(instrument: &Instrument) -> ExchangeError {
    ExchangeError::Unavailable {
        reason: format!("the exchange does not list `{}`", instrument.symbol()),
    }
}

/// 本地就没能签名：订单根本没发出去，按拒绝处理，不留悬空的预留。
fn local_failure(error: &crate::signing::SigningError) -> ExchangeError {
    ExchangeError::Rejected {
        reason: format!("the order was not sent: {error}"),
    }
}

fn request_type(body: &Value) -> &str {
    body.get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use crate::testing::{FakeTransport, advanceable_clock, client, fake_client};

    #[test]
    fn nonces_increase_even_within_the_same_millisecond() {
        let client = fake_client(1_700_000_000_000);

        let first = client.next_nonce();
        let second = client.next_nonce();
        let third = client.next_nonce();

        assert_eq!(first, 1_700_000_000_000);
        assert_eq!(second, first + 1, "同一毫秒内的两单不能共用 nonce");
        assert_eq!(third, first + 2);
    }

    #[test]
    fn nonces_follow_the_clock_when_it_moves_forward() {
        let (now, clock) = advanceable_clock(1_700_000_000_000);
        let client = client(FakeTransport::new(Vec::new()), clock);

        assert_eq!(client.next_nonce(), 1_700_000_000_000);
        now.store(1_700_000_000_500, Ordering::SeqCst);
        assert_eq!(client.next_nonce(), 1_700_000_000_500);
    }
}
