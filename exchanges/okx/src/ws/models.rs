use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WsArg {
    pub channel: String,
    #[serde(rename = "instId")]
    pub inst_id: Option<String>,
    #[serde(rename = "instType")]
    pub inst_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MarketMessage {
    pub arg: WsArg,
    #[serde(default)]
    pub data: Vec<MarketData>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MarketData {
    #[serde(rename = "instId")]
    pub inst_id: String,
    #[serde(rename = "bidPx")]
    pub bid_px: Option<String>,
    #[serde(rename = "askPx")]
    pub ask_px: Option<String>,
    #[serde(rename = "markPx")]
    pub mark_px: Option<String>,
    pub ts: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserMessage {
    pub arg: WsArg,
    #[serde(default)]
    pub data: Vec<serde_json::Value>,
}
