use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum ExchangeAction {
    Order(OrderAction),
    Cancel(CancelAction),
    UpdateLeverage(UpdateLeverageAction),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OrderAction {
    pub orders: Vec<OrderRequest>,
    pub grouping: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builder: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
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
    #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
    pub cloid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum OrderType {
    Limit(LimitOrderType),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct LimitOrderType {
    pub tif: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CancelAction {
    pub cancels: Vec<CancelRequest>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CancelRequest {
    #[serde(rename = "a")]
    pub asset: u32,
    #[serde(rename = "o")]
    pub oid: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateLeverageAction {
    pub asset: u32,
    pub is_cross: bool,
    pub leverage: u32,
}

#[cfg(test)]
mod tests {
    use crate::rest::actions::{
        CancelAction, CancelRequest, ExchangeAction, LimitOrderType, OrderAction, OrderRequest,
        OrderType, UpdateLeverageAction,
    };
    use crate::signing::{HyperliquidChain, action_hash, sign_l1_action};

    fn private_key() -> &'static str {
        "e908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e"
    }

    #[test]
    fn order_action_hash_matches_hyperliquid_sdk_signature_sample() {
        let action = ExchangeAction::Order(OrderAction {
            orders: vec![OrderRequest {
                asset: 1,
                is_buy: true,
                limit_px: "2000.0".to_string(),
                sz: "3.5".to_string(),
                reduce_only: false,
                order_type: OrderType::Limit(LimitOrderType {
                    tif: "Ioc".to_string(),
                }),
                cloid: Some("0x1e60610f0b3d420597c88c1fed2ad5ee".to_string()),
            }],
            grouping: "na".to_string(),
            builder: None,
        });

        let connection_id = action_hash(&action, 1_583_838, None).unwrap();
        let mainnet = sign_l1_action(private_key(), HyperliquidChain::Mainnet, connection_id)
            .unwrap()
            .to_compact_hex();
        let testnet = sign_l1_action(private_key(), HyperliquidChain::Testnet, connection_id)
            .unwrap()
            .to_compact_hex();

        assert_eq!(
            mainnet,
            "d3e894092eb27098077145714630a77bbe3836120ee29df7d935d8510b03a08f456de5ec1be82aa65fc6ecda9ef928b0445e212517a98858cfaa251c4cd7552b1c"
        );
        assert_eq!(
            testnet,
            "3768349dbb22a7fd770fc9fc50c7b5124a7da342ea579b309f58002ceae49b4357badc7909770919c45d850aabb08474ff2b7b3204ae5b66d9f7375582981f111c"
        );
    }

    #[test]
    fn cancel_action_hash_matches_hyperliquid_sdk_signature_sample() {
        let action = ExchangeAction::Cancel(CancelAction {
            cancels: vec![CancelRequest {
                asset: 1,
                oid: 82382,
            }],
        });

        let connection_id = action_hash(&action, 1_583_838, None).unwrap();
        let mainnet = sign_l1_action(private_key(), HyperliquidChain::Mainnet, connection_id)
            .unwrap()
            .to_compact_hex();
        let testnet = sign_l1_action(private_key(), HyperliquidChain::Testnet, connection_id)
            .unwrap()
            .to_compact_hex();

        assert_eq!(
            mainnet,
            "02f76cc5b16e0810152fa0e14e7b219f49c361e3325f771544c6f54e157bf9fa17ed0afc11a98596be85d5cd9f86600aad515337318f7ab346e5ccc1b03425d51b"
        );
        assert_eq!(
            testnet,
            "6ffebadfd48067663390962539fbde76cfa36f53be65abe2ab72c9db6d0db44457720db9d7c4860f142a484f070c84eb4b9694c3a617c83f0d698a27e55fd5e01c"
        );
    }

    #[test]
    fn update_leverage_action_serializes_wire_shape() {
        let action = ExchangeAction::UpdateLeverage(UpdateLeverageAction {
            asset: 1,
            is_cross: true,
            leverage: 10,
        });

        let value = serde_json::to_value(action).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "type": "updateLeverage",
                "asset": 1,
                "isCross": true,
                "leverage": 10
            })
        );
    }
}
