//! 发往 `/exchange` 的动作。
//!
//! 字段名按 Hyperliquid 的线上格式来：撮合引擎读的是缩写字段（`a`/`b`/`p`/`s`/`r`/`t`/`c`），
//! msgpack 编码后再签名，所以这里的字段名和顺序都必须与交易所一致。顺序不影响
//! msgpack 的 map 语义，但字段名错一个字节签名就废了。

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum ExchangeAction {
    Order(OrderAction),
    CancelByCloid(CancelByCloidAction),
}

impl ExchangeAction {
    /// 幂等用的动作类型，写日志用。
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Order(_) => "order",
            Self::CancelByCloid(_) => "cancelByCloid",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OrderAction {
    pub orders: Vec<OrderRequest>,
    /// 一个动作里只放一张单时用 `na`。
    pub grouping: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OrderRequest {
    #[serde(rename = "a")]
    pub asset: u32,
    #[serde(rename = "b")]
    pub is_buy: bool,
    #[serde(rename = "p")]
    pub limit_px: String,
    #[serde(rename = "s")]
    pub sz: String,
    #[serde(rename = "r")]
    pub reduce_only: bool,
    #[serde(rename = "t")]
    pub order_type: OrderType,
    #[serde(rename = "c")]
    pub cloid: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OrderType {
    Limit(LimitOrderType),
    Trigger(TriggerOrderType),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LimitOrderType {
    /// 有效期策略：`Gtc` 一直挂到撤销。
    pub tif: &'static str,
}

/// 触发单：标记价越过 `triggerPx` 时交易所发出订单。字段顺序和官方 SDK 一致
/// （`isMarket`、`triggerPx`、`tpsl`），msgpack 签名按这个顺序编码。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct TriggerOrderType {
    #[serde(rename = "isMarket")]
    pub is_market: bool,
    #[serde(rename = "triggerPx")]
    pub trigger_px: String,
    /// `sl` 止损、`tp` 止盈。
    pub tpsl: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CancelByCloidAction {
    pub cancels: Vec<CancelByCloid>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CancelByCloid {
    pub asset: u32,
    pub cloid: String,
}

#[cfg(test)]
mod tests {
    use super::{
        CancelByCloid, CancelByCloidAction, ExchangeAction, LimitOrderType, OrderAction,
        OrderRequest, OrderType, TriggerOrderType,
    };
    use crate::signing::{action_hash, sign_l1_action};
    use poise_runtime::{Network, SecretKey};

    /// 上游 SDK 示例里用的私钥与 nonce。
    const PRIVATE_KEY: &str = "0xe908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e";
    const NONCE: u64 = 1_583_838;

    fn signed(action: &ExchangeAction, network: Network) -> String {
        let key = SecretKey::parse(PRIVATE_KEY).unwrap();
        sign_l1_action(&key, network, action_hash(action, NONCE, None).unwrap())
            .unwrap()
            .to_compact_hex()
    }

    /// 用官方 SDK 的示例动作签名，比对公开的签名样本。
    ///
    /// 期望值取自 poise 旧实现 `exchanges/hyperliquid/src/rest/actions.rs`，它当时
    /// 对齐过 Hyperliquid 官方 SDK。改字段名、改编码方式都会让它失败。
    #[test]
    fn order_action_matches_the_published_sdk_signature() {
        let action = ExchangeAction::Order(OrderAction {
            orders: vec![OrderRequest {
                asset: 1,
                is_buy: true,
                limit_px: "2000.0".to_string(),
                sz: "3.5".to_string(),
                reduce_only: false,
                order_type: OrderType::Limit(LimitOrderType { tif: "Ioc" }),
                cloid: "0x1e60610f0b3d420597c88c1fed2ad5ee".to_string(),
            }],
            grouping: "na",
        });

        assert_eq!(
            signed(&action, Network::Mainnet),
            "d3e894092eb27098077145714630a77bbe3836120ee29df7d935d8510b03a08f456de5ec1be82aa65fc6ecda9ef928b0445e212517a98858cfaa251c4cd7552b1c"
        );
        assert_eq!(
            signed(&action, Network::Testnet),
            "3768349dbb22a7fd770fc9fc50c7b5124a7da342ea579b309f58002ceae49b4357badc7909770919c45d850aabb08474ff2b7b3204ae5b66d9f7375582981f111c"
        );
    }

    #[test]
    fn the_same_action_signs_differently_on_each_network() {
        let action = ExchangeAction::CancelByCloid(CancelByCloidAction {
            cancels: vec![CancelByCloid {
                asset: 1,
                cloid: "0x1e60610f0b3d420597c88c1fed2ad5ee".to_string(),
            }],
        });

        let mainnet = signed(&action, Network::Mainnet);
        let testnet = signed(&action, Network::Testnet);

        assert_ne!(mainnet, testnet, "两个网络的 source 不同，签名必须不同");
        assert_eq!(mainnet.len(), 130);
    }

    #[test]
    fn order_action_serializes_the_wire_shape() {
        let action = ExchangeAction::Order(OrderAction {
            orders: vec![OrderRequest {
                asset: 3,
                is_buy: false,
                limit_px: "95000".to_string(),
                sz: "0.001".to_string(),
                reduce_only: true,
                order_type: OrderType::Limit(LimitOrderType { tif: "Gtc" }),
                cloid: "0x11111111111111111111111111111111".to_string(),
            }],
            grouping: "na",
        });

        assert_eq!(
            serde_json::to_value(&action).unwrap(),
            serde_json::json!({
                "type": "order",
                "orders": [{
                    "a": 3,
                    "b": false,
                    "p": "95000",
                    "s": "0.001",
                    "r": true,
                    "t": {"limit": {"tif": "Gtc"}},
                    "c": "0x11111111111111111111111111111111"
                }],
                "grouping": "na"
            })
        );
        assert_eq!(action.kind(), "order");
    }

    /// 止损触发单的线上格式：`t` 是 `{"trigger": {"isMarket", "triggerPx", "tpsl"}}`。
    #[test]
    fn stop_order_serializes_the_wire_shape() {
        let action = ExchangeAction::Order(OrderAction {
            orders: vec![OrderRequest {
                asset: 0,
                is_buy: false,
                limit_px: "54000".to_string(),
                sz: "0.002".to_string(),
                reduce_only: true,
                order_type: OrderType::Trigger(TriggerOrderType {
                    is_market: true,
                    trigger_px: "60000".to_string(),
                    tpsl: "sl",
                }),
                cloid: "0x33333333333333333333333333333333".to_string(),
            }],
            grouping: "na",
        });

        assert_eq!(
            serde_json::to_string(&action).unwrap(),
            r#"{"type":"order","orders":[{"a":0,"b":false,"p":"54000","s":"0.002","r":true,"t":{"trigger":{"isMarket":true,"triggerPx":"60000","tpsl":"sl"}},"c":"0x33333333333333333333333333333333"}],"grouping":"na"}"#
        );
    }

    #[test]
    fn cancel_action_serializes_the_wire_shape() {
        let action = ExchangeAction::CancelByCloid(CancelByCloidAction {
            cancels: vec![CancelByCloid {
                asset: 0,
                cloid: "0x22222222222222222222222222222222".to_string(),
            }],
        });

        assert_eq!(
            serde_json::to_value(&action).unwrap(),
            serde_json::json!({
                "type": "cancelByCloid",
                "cancels": [{
                    "asset": 0,
                    "cloid": "0x22222222222222222222222222222222"
                }]
            })
        );
        assert_eq!(action.kind(), "cancelByCloid");
    }
}
