use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicTickerMessage {
    pub topic: String,
    pub ts: i64,
    pub data: PublicTickerData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicTickerData {
    #[serde(default)]
    pub mark_price: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrderTopicMessage {
    pub topic: String,
    pub creation_time: i64,
    pub data: Vec<OrderUpdate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrderUpdate {
    #[serde(deserialize_with = "deserialize_string")]
    pub symbol: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub order_id: String,
    #[serde(default)]
    pub order_link_id: Option<String>,
    #[serde(deserialize_with = "deserialize_string")]
    pub side: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub price: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub qty: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub order_status: String,
    #[serde(default)]
    pub stop_order_type: Option<String>,
    #[serde(deserialize_with = "deserialize_i64")]
    pub position_idx: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PositionTopicMessage {
    pub topic: String,
    pub creation_time: i64,
    pub data: Vec<PositionUpdate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PositionUpdate {
    #[serde(deserialize_with = "deserialize_string")]
    pub symbol: String,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(deserialize_with = "deserialize_string")]
    pub size: String,
    #[serde(rename = "entryPrice", deserialize_with = "deserialize_string")]
    pub entry_price: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub unrealised_pnl: String,
    #[serde(deserialize_with = "deserialize_i64")]
    pub position_idx: i64,
}

pub(crate) fn deserialize_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        other => Err(Error::custom(format!(
            "expected string or number, got {other}"
        ))),
    }
}

pub(crate) fn deserialize_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(value) => value
            .as_i64()
            .ok_or_else(|| Error::custom("expected integer number")),
        serde_json::Value::String(value) => value
            .parse::<i64>()
            .map_err(|error| Error::custom(format!("invalid integer `{value}`: {error}"))),
        other => Err(Error::custom(format!("expected integer, got {other}"))),
    }
}
