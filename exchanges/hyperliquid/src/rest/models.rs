use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct MetaResponse {
    pub universe: Vec<PerpAssetMeta>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct PerpAssetMeta {
    pub name: String,
    #[serde(rename = "szDecimals")]
    pub sz_decimals: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ClearinghouseStateResponse {
    #[serde(rename = "marginSummary")]
    pub margin_summary: MarginSummary,
    pub withdrawable: String,
    #[serde(rename = "assetPositions")]
    pub asset_positions: Vec<AssetPosition>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct MarginSummary {
    #[serde(rename = "accountValue")]
    pub account_value: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct AssetPosition {
    pub position: PositionData,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct PositionData {
    pub coin: String,
    pub szi: String,
    #[serde(rename = "entryPx")]
    pub entry_px: Option<String>,
    #[serde(rename = "unrealizedPnl")]
    pub unrealized_pnl: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct OpenOrderResponse {
    pub coin: String,
    pub oid: u64,
    pub cloid: Option<String>,
    pub side: String,
    #[serde(rename = "limitPx")]
    pub limit_px: String,
    pub sz: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_meta_response() {
        let response: MetaResponse =
            serde_json::from_str(r#"{"universe":[{"name":"BTC","szDecimals":5}]}"#).unwrap();

        assert_eq!(
            response,
            MetaResponse {
                universe: vec![PerpAssetMeta {
                    name: "BTC".to_string(),
                    sz_decimals: 5,
                }],
            }
        );
    }

    #[test]
    fn deserializes_clearinghouse_state_response() {
        let response: ClearinghouseStateResponse = serde_json::from_str(
            r#"{"marginSummary":{"accountValue":"125.5"},"withdrawable":"100.25","assetPositions":[{"position":{"coin":"BTC","szi":"0.02","entryPx":"65000.5","unrealizedPnl":"3.25"}}]}"#,
        )
        .unwrap();

        assert_eq!(response.margin_summary.account_value, "125.5");
        assert_eq!(response.withdrawable, "100.25");
        assert_eq!(response.asset_positions[0].position.coin, "BTC");
        assert_eq!(
            response.asset_positions[0].position.entry_px.as_deref(),
            Some("65000.5")
        );
    }

    #[test]
    fn deserializes_open_order_response() {
        let response: OpenOrderResponse = serde_json::from_str(
            r#"{"coin":"BTC","oid":12345,"cloid":"0x11111111111111111111111111111111","side":"B","limitPx":"65000.5","sz":"0.02"}"#,
        )
        .unwrap();

        assert_eq!(response.oid, 12345);
        assert_eq!(response.cloid.as_deref(), Some("0x11111111111111111111111111111111"));
        assert_eq!(response.limit_px, "65000.5");
    }
}
